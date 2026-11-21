// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An extent store for a guest's own filesystem, Fxfs in its kernel: what a VMM's virtio
//! extent-store device serves (backlog doc-4), through the extent daemon (`crate::cladefsd`).
//!
//! An [`OpenCell`] is an image (`crates/local_cell`) whose extents the guest writes, reads and
//! frees by ID. Its I/O copies between the image's files and guest memory, and blocks the calling
//! thread: a device's queue threads call it.
//!
//! Sealed extents are cached in physical memory (`crate::cache`), keyed by extent ID, which every
//! store and window in the process shares: each a table of its pages, which are read in from the
//! store as they are first read or mapped, and evicted as memory runs out. Windows map them
//! read-only: one copy of each page, whichever store or fork maps it, as an extent never changes
//! once sealed, its ID its cell's to write once, which the store keeps to (TASK-79.6). Its pages
//! are read from any of the stores that asked for it, open still, that hold it (TASK-79.7). Reads
//! of sealed extents read it too (TASK-32): the guest's filesystem reads its metadata, which it
//! maps not, so a fork reads what its base's other forks read before from memory, not the store.
//! Reads of open extents, and a follower's, go to the store. The store's files bypass the host's
//! page cache.
//!
//! cladefs: windows map sealed extents and zeros alone, read-only (backlog doc-13): an open extent
//! is written only by `WRITE`, and a `MAP` of one fails (`EPERM`). A writer's window copies on
//! write: a store to a page gives the window a private copy of it (`crate::dax`), which no extent
//! names, so that no commit changes anything in a window.

use crate::cache::{self, CachedExtent, CachedIn};
use crate::cell_dir;
use crate::dax::{Dax, DaxMapping, DaxWindow, SecondStage, Source};
use crate::error::{Errno, Error, Result};
use crate::faults::failpoint;
use crate::halt::{CellGuard, CellMutex, Halt, StoreRwLock};
use crate::host_cell::{self, CellOf};
use crate::memory::{VolatileSlice, VolatileSliceMut, gather, scatter};
use crate::pages::{Arena, ArenaId, page_size};
use crate::profile::{self, Recorder};
use crate::stats::{self, Counter};
use bucket_cell::cell::Kind;
use bucket_cell::layout::CellId;
use bucket_cell::trace;
use local_cell::{
    HostDir, LocalCell, NoSpace, NotHeld, NotQuiescent, SpentExtent, TooLong, TooSmall, Unaligned,
    Unsectored,
};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use storage_device::{ExtentId, NoExtent, OtherCell, SealedExtent};

/// What a store holds, as doc-4's `INFO` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtentInfo {
    /// The most bytes the store's extents may hold.
    pub capacity: u64,
    /// The bytes its extents hold.
    pub used: u64,
    /// The bytes it can still take.
    pub available: u64,
    /// One more than the highest sequence number of the extents the store's cell holds, as of
    /// its opening, and of those written since: where fxfs looks for orphans below. A number
    /// freed past it is minted no more, its writer minting from [`Info::sequence_floor`]
    /// (TASK-165.9).
    pub next_sequence: u64,
    /// cladefs: the lowest sequence number the guest may mint from: its writer's range's start, with
    /// an uploader (doc-7, The head), past which the store need hold nothing.
    pub sequence_floor: u64,
}

/// cladefs: what a store's cell holds, and whose it is: the bytes its extents hold (TASK-49), what
/// billing a store alone charges it and what it shares with the cells it was forked from; and
/// how many extents they are (TASK-454), which what an open store holds in memory grows with
/// (TASK-443.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellHolding {
    /// The bytes its extents hold.
    pub bytes: Whose,
    /// How many extents it holds.
    pub extents: Whose,
}

/// cladefs: a count of what a store holds ([`CellHolding`]), split by whose it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Whose {
    /// Of the extents its cell wrote.
    pub own: u64,
    /// Of the extents it inherited from the cells it was forked from, which may still share
    /// them.
    pub inherited: u64,
}

/// A mapping of a store's DAX window, as a snapshot of its VM records it, to map it again where
/// the VM is restored ([`OpenCell::window_mappings`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowMapping {
    /// Where in the window it is.
    pub window_offset: u64,
    /// How many bytes it maps.
    pub len: u64,
    /// What it maps.
    pub contents: MappingContents,
}

/// What a [`WindowMapping`] maps, read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingContents {
    /// A sealed extent's bytes.
    Extent {
        /// The extent.
        extent: ExtentId,
        /// Where in it the bytes mapped start.
        offset: u64,
    },
    /// Zeros.
    Zeros,
}

/// cladefs: what a store opened is: who writes it, and whether every tenant reads it. It says what
/// may be done to it, which each of its methods that changes it checks, rather than each caller
/// (TASK-160.10). Fixed once it is open, with its uploader, which says whether it is a version, as
/// the extent daemon has it before anything asks it to do anything (`cladefsd::cells`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellAccess {
    /// Who writes it.
    pub role: CellRole,
    /// Whether its cell is public ([`OpenCell::make_public`]): a base every tenant's VM
    /// processes fork, its pages the default arena's, which each holds. As its directory said as
    /// it opened, which fails if it can't be read: a store made public is so to those opened since.
    pub public: bool,
}

/// cladefs: who writes a store ([`CellAccess`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellRole {
    /// Its writer: a guest's filesystem, or the work on it of whatever opened it.
    Writer,
    /// None: it is a follower's (backlog doc-8), reading its writer's commits through a view.
    Follower,
    /// None: it is a version (doc-7, Versions), the name of a state, which any number of hosts read
    /// and fork at once.
    Version,
}

impl CellAccess {
    /// `Ok` if what the store holds may change: its extents and commits, which only its writer's
    /// do, and of no public base. A follower's and a version refuse with `EROFS`; a
    /// public base with `EPERM`, as what changed would be every tenant's that forks it (TASK-43).
    pub fn may_write(self) -> Result<()> {
        match self {
            Self { role: CellRole::Follower, .. } => Err(Errno::EROFS),
            Self { public: true, .. } => Err(Errno::EPERM),
            Self { role: CellRole::Version, .. } => Err(Errno::EROFS),
            Self { role: CellRole::Writer, public: false } => Ok(()),
        }
    }

    /// `Ok` if the store's capacity may change, its writer's alone: `EROFS` for a follower's, and
    /// for a version, whose forks take it as it is. A public base's grows, as no fork it made shares
    /// it.
    pub fn may_resize(self) -> Result<()> {
        match self.role {
            CellRole::Writer => Ok(()),
            CellRole::Follower | CellRole::Version => Err(Errno::EROFS),
        }
    }

    /// `Ok` if the store may be forked, made public or have a base attached, which changes none of
    /// its extents, as a version and a public base may: `EROFS` for a follower's, whose view is all
    /// it has of the store.
    pub fn may_manage(self) -> Result<()> {
        match self.role {
            CellRole::Follower => Err(Errno::EROFS),
            CellRole::Writer | CellRole::Version => Ok(()),
        }
    }
}

/// cladefs: what a request changes of a store, which [`OpenCell::admit`] admits it to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Changing {
    /// What it holds: its extents and commits ([`CellAccess::may_write`]).
    Contents,
    /// Its capacity ([`CellAccess::may_resize`]).
    Capacity,
    /// Its cell, not what it holds: its versions, forks, publicity and bases
    /// ([`CellAccess::may_manage`]).
    Cell,
}

/// An extent store a guest's filesystem lives on. See the module docs.
pub struct OpenCell {
    store: Arc<LocalCell<HostDir>>,
    /// cladefs: the bucket the store's description names, which its cell is kept in
    /// (`host_cell::Opened`, TASK-165.6).
    bucket: Arc<bucket_cell::object_bucket::ObjectBucket>,
    /// cladefs: where it was opened: its directory, which keeps what is attached to it (backlog
    /// doc-10), and what it was forked from.
    path: std::path::PathBuf,
    /// cladefs: the forks of other stores attached to it, whose extents it reads through to.
    bases: Arc<Bases>,
    dax: CellMutex<Option<Dax>>,
    /// cladefs: the arenas of physical memory its extents' pages are of (TASK-43).
    arenas: Arenas,
    /// cladefs: the stores it was forked from, nearest first, as of the forks ([`cell_dir::ancestry`]).
    forked_from: Arc<Vec<(u64, std::path::PathBuf)>>,
    /// cladefs: the uploader of the store's cell to object storage, which opened it, taking its
    /// head, or holds its version: what doc-4's `COMMIT_UPLOADED` commits wait for.
    /// None for a follower, whose writer's it is (TASK-165.9).
    uploader: Option<local_cell::upload::Uploader<HostDir>>,
    /// cladefs: of an ephemeral cell's store, its keys held among the host's while it is open, as an
    /// uploader holds a bucket cell's (doc-25, TASK-698.7).
    keys: Option<bucket_cell::keys::RingHold>,
    /// cladefs: what records the store's page-ins for its base's boot profile, if it is a fork of a
    /// base that has none yet, and what writes it at the window's end, which closing stops early.
    profiling: Option<(Arc<Recorder>, tokio::sync::oneshot::Sender<()>)>,
    /// cladefs: of a store opened for a guest, a fork of a version, its version's read evidence of
    /// its tenant's: what predicts its reads, and a recording of them, if it records
    /// (`crate::evidence`, TASK-702.2).
    evidence: Option<crate::evidence::Evidence>,
    /// cladefs: held, shared, by writes, which open extents, and alone by a fork only once nothing
    /// is open, which none comes between ([`OpenCell::fork_quiescent`]).
    opening: StoreRwLock<()>,
    /// cladefs: a follower's (backlog doc-8), what it follows ([`OpenCell::follow`]).
    following: Option<Following>,
    /// cladefs: notified as it commits, for its followers' daemon to tell them, holding no thread
    /// meanwhile (TASK-34).
    committed: event_listener::Event,
    /// cladefs: the window's latest changes, which a refused fault's log tells of the page it was
    /// on ([`OpenCell::refused`], TASK-44.12).
    changes: CellMutex<WindowChanges>,
    /// cladefs: the turns its daemon has taken at making its commits local behind them
    /// ([`OpenCell::flush_turned`]): how a commit's lag is told in turns, which the daemon's
    /// load stretches, rather than in time (TASK-110).
    flush_turns: std::sync::atomic::AtomicU64,
    /// cladefs: what each of its locks is locked through: set by a panic holding one, after which it
    /// serves nothing more (`crate::halt`, TASK-153.16).
    halt: Halt,
    /// cladefs: whether it was closed ([`OpenCell::close`]), which its drop then does again not.
    closed: bool,
    /// cladefs: what reads its extents whose data is in the bucket: of the bucket its uploader keeps
    /// the cell in, keeping what it fetches in the host's cache, if it has one.
    fetcher: Arc<crate::bucket::HostFetcher>,
}

/// cladefs: a version of a cell, as a VM snapshot holds it and its restores fork it
/// (`CellClient::make_version`, `DaemonConnection::open_forked`): the cell's tenant, its
/// ID and the version's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CellVersion {
    /// The cell's tenant.
    pub tenant: u64,
    /// The cell's ID, its cell's.
    pub cell: u64,
    /// The version's ID.
    pub version: u64,
}

/// cladefs: a version of a store's cell ([`OpenCell::versions`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionInfo {
    /// Its ID, unique in its cell.
    pub id: u64,
    /// Whether the cell's head names it: forks of it on other hosts are made then.
    pub uploaded: bool,
    /// Whether forks of it into other tenants' cells are made.
    pub shared: bool,
}

/// cladefs: how a store is opened ([`OpenCell::open_with`], TASK-165.9).
pub struct Opening {
    /// The arena of physical memory its own extents' pages are of; the default arena's if none.
    pub arena: Option<Arc<Arena>>,
    /// Whether its base's boot profile is read into the extent cache, or recorded
    /// ([`OpenCell::profile`]).
    pub profiled: bool,
    /// How its uploader keeps its cell in its bucket, taking its head.
    pub keeping: local_cell::upload::Options,
}

impl Opening {
    /// As this process opens stores where no host's daemon serves them, as [`crate::Storage`]
    /// does: a copy never placed taking its cell with this host's token, or the first
    /// placement's (`host_cell::keeping`); its pages the default arena's, its profile read or
    /// recorded.
    pub fn here() -> Result<Self, Error> {
        let keeping = host_cell::keeping()?;
        Ok(Self { arena: None, profiled: true, keeping })
    }
}

/// cladefs: the `MAP`s of open extents refused (backlog doc-13), which no guest makes: a tripwire,
/// which the daemon's metrics show.
static OPEN_MAPS_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// cladefs: the `MAP`s of open extents refused so far.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn open_maps_refused() -> u64 {
    OPEN_MAPS_REFUSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// cladefs: refuses a `MAP` of `extent`, open: counts and logs it (each of the first thousand, and
/// every power of two after), and returns `EPERM`.
fn refuse_open_map(extent: ExtentId) -> Errno {
    let count = OPEN_MAPS_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if count <= 1000 || count.is_power_of_two() {
        log::warn!(
            "cladefs: a MAP of {extent:?}, open, refused: windows map sealed extents alone ({count} \
             so far)"
        );
    }
    Errno::EPERM
}

/// cladefs: the latest of a window's MAPs and UNMAPs, as [`WindowChanges::MOST`] of them: which
/// mapped a page as it was refused, and what mapped it before (TASK-44.12).
#[derive(Default)]
struct WindowChanges {
    changes: std::collections::VecDeque<WindowChange>,
    /// cladefs: the pages whose accesses the window refused, until a change of the window covers
    /// them, which is logged then, with what it did (TASK-44.12).
    refused: Vec<Refused>,
}

/// A page whose access the window refused ([`WindowChanges::refused`]): when, and what the window
/// mapped there then.
struct Refused {
    at: std::time::Instant,
    window_offset: u64,
    what: String,
    mapped: String,
}

/// One of a window's changes: `len` bytes at `window_offset`, mapped or unmapped, when.
struct WindowChange {
    at: std::time::Instant,
    window_offset: u64,
    len: u64,
    change: Change,
}

/// What a [`WindowChange`] did.
enum Change {
    /// A MAP of `extent`'s bytes from `offset`, and how it ended.
    Map {
        extent: ExtentId,
        offset: u64,
        result: std::result::Result<(), Errno>,
    },
    /// A MAP of zeros.
    Zeros,
    /// Zeros mapped in place of what a freed extent's memory, which they kept, mapped.
    Freed,
    Unmap,
}

impl Change {
    /// What the change did, said for a log.
    fn describe(&self) -> String {
        match self {
            Change::Map { extent, offset, result } => {
                format!("MAP of {extent:?} from {offset:#x}, {result:?}")
            }
            Change::Zeros => "MAP of zeros".into(),
            Change::Freed => "zeros in place of a freed extent's".into(),
            Change::Unmap => "UNMAP".into(),
        }
    }
}

impl WindowChanges {
    /// cladefs: enough that a refused access's page's history reaches back past the many MAPs of a
    /// busy guest (TASK-121).
    const MOST: usize = 1 << 16;

    /// The most refused pages awaiting a change ([`WindowChanges::refused`]): the oldest go first.
    const MOST_REFUSED: usize = 1024;

    fn record(&mut self, window_offset: u64, len: u64, change: Change) {
        if self.changes.len() == Self::MOST {
            self.changes.pop_front();
        }
        let at = std::time::Instant::now();
        if !self.refused.is_empty() {
            // What a VM process asked, which may be refused yet: past the end of a u64.
            let range = window_offset..window_offset.saturating_add(len);
            self.refused.retain(|refused| {
                if !range.contains(&refused.window_offset) {
                    return true;
                }
                log::warn!(
                    "cladefsd: {} refused at DAX window offset {:#x}, {:?} ago, mapping {}, \
                     met: {}, {len:#x} bytes from {window_offset:#x}",
                    refused.what,
                    refused.window_offset,
                    at.duration_since(refused.at),
                    refused.mapped,
                    change.describe(),
                );
                false
            });
        }
        self.changes.push_back(WindowChange { at, window_offset, len, change });
    }

    /// Notes the page at `window_offset` refused `what`, the window mapping `mapped` there: the
    /// next change covering it is logged.
    fn refuse(&mut self, window_offset: u64, what: &str, mapped: String) {
        if self.refused.len() == Self::MOST_REFUSED {
            self.refused.remove(0);
        }
        let at = std::time::Instant::now();
        self.refused.push(Refused { at, window_offset, what: what.to_owned(), mapped });
    }

    /// The changes of the page at `window_offset`, oldest first, with how long ago each was.
    fn of(&self, window_offset: u64) -> Vec<String> {
        let now = std::time::Instant::now();
        (self.changes.iter())
            .filter(|change| {
                let end = change.window_offset.saturating_add(change.len);
                (change.window_offset..end).contains(&window_offset)
            })
            .map(|change| {
                let (at, len, ago) =
                    (change.window_offset, change.len, now.duration_since(change.at));
                format!("{}, {len:#x} bytes from {at:#x}, {ago:?} ago", change.change.describe())
            })
            .collect()
    }
}

/// A follower's part of its store: the writer's store it follows, and the view it reads it
/// through.
struct Following {
    writer: Arc<OpenCell>,
    view: CellMutex<local_cell::View>,
}

/// The barriers a write is ordered with (doc-4's `WRITE` flags).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteBarriers<'a> {
    /// A barrier precedes the write (`WRITE_BARRIER`).
    pub before: bool,
    /// A commit follows it (`WRITE_COMMIT`), which seals the extent it writes too.
    pub after: Option<Commit>,
    /// The root that commit sets, if any (`WRITE_ROOT`), as a `COMMIT`'s with `COMMIT_ROOT` does:
    /// a sync's journal write, whose end the guest's filesystem records there (TASK-153.13).
    pub root: Option<&'a [u8]>,
}

/// When a commit completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commit {
    /// Once ordered: the store makes it local soon after.
    Ordered,
    /// cladefs: once local, synced to the host's disk, not waiting for the bucket (`COMMIT_LOCAL`):
    /// what a database's commit waits for, as SQLite's own does on a local filesystem (TASK-301).
    Local,
    /// Once uploaded (`COMMIT_UPLOADED`): its segments acknowledged by the bucket, if the store has
    /// one; local, if not.
    Uploaded,
}

impl Commit {
    /// Its level's name, as doc-12 and the traces say it.
    pub fn level(self) -> &'static str {
        match self {
            Commit::Ordered => "ordered",
            Commit::Local => "local",
            Commit::Uploaded => "uploaded",
        }
    }
}

/// cladefs: the errno of the uploader's failure (`Failure::errno`): `EIO`, logged, for one the
/// store failed to read or write.
fn upload_errno(failure: local_cell::upload::Failure) -> Errno {
    crate::bucket::errno(&failure).unwrap_or_else(|| {
        log::error!("cladefs: uploading the store's commits: {failure}");
        Errno::EIO
    })
}

/// cladefs: lets `store` go, and, if this was its last reference, closes it ([`OpenCell::close`]),
/// holding no thread as it waits: as every holder of the daemon's stores lets one go (doc-24, step
/// 9), so that whichever holds it last, a connection's teardown, a request, a fork or the store
/// following it, closes it, awaited, and none closes it in its drop, which blocks.
pub async fn release(store: Arc<OpenCell>) {
    if let Some(store) = Arc::into_inner(store) {
        // Boxed: a store's close releases its writer and bases, which release theirs.
        Box::pin(store.close()).await;
    }
}

/// cladefs: the span of a commit's wait for object storage to hold the commits up to `epoch`, or
/// as many of them as its lag allows (`bucket_cell::trace`, TASK-258).
fn upload_wait(epoch: u64) -> trace::Span {
    let span = trace::Span::start("commit.upload_wait");
    span.set_count(trace::COMMIT, epoch);
    span
}

/// The errno of the store's error, as doc-4 has them: each of the store's errors its own, and any
/// other the store failing to read or write, `EIO`, logged. The one list of them, which a
/// writer's requests and a follower's share.
pub(crate) fn errno(error: anyhow::Error) -> Errno {
    if error.is::<local_cell::CutOff>() {
        // A follower too far behind its writer (doc-8, decision 7): it remounts.
        Errno::ESTALE
    } else if let Some(NotHeld(extent)) = error.downcast_ref::<NotHeld>() {
        // cladefs: a follower's request for an extent its view's commits don't hold and it doesn't
        // pin: a bug of its guest's follower, never a stale read (doc-8, testing 1).
        log::error!("cladefs: a follower's request for {extent:?}, outside its view: {error:#}");
        Errno::EIO
    } else if error.is::<local_cell::TooManyPins>() {
        // cladefs: a follower pinning more than it may hold open (TASK-588).
        Errno::EMFILE
    } else if error.is::<NoExtent>() {
        // A follower's too, of one written after its view's commit: as Fxfs's journal replay
        // finds the end of the commit.
        Errno::ENOENT
    } else if error.is::<SealedExtent>() || error.is::<SpentExtent>() || error.is::<OtherCell>() {
        Errno::EPERM
    } else if error.is::<NoSpace>()
        || error.is::<TooSmall>()
        || error.is::<local_cell::footprint::DiskFull>()
    {
        // cladefs: and its disk, or its tenant's limits, refusing it (doc-22).
        Errno::ENOSPC
    } else if error.is::<Unaligned>() || error.is::<Unsectored>() || error.is::<TooLong>() {
        Errno::EINVAL
    } else if full_disk(&error) {
        // cladefs: the host's disk refusing what the store makes for the request, a raw file or a
        // segment's, its admission past (doc-22, TASK-539.12): a full disk to the guest, as its
        // admission's refusal is, not an I/O error. A store failed says so instead, and has none.
        Errno::ENOSPC
    } else {
        log::error!("cladefs: extent store: {error:#}");
        Errno::EIO
    }
}

/// cladefs: whether `error` is the host's disk saying it is full.
fn full_disk(error: &anyhow::Error) -> bool {
    error.chain().filter_map(|cause| cause.downcast_ref::<std::io::Error>()).any(|err| {
        err.raw_os_error() == Some(libc::ENOSPC) || err.kind() == std::io::ErrorKind::StorageFull
    })
}

// cladefs: closing the store frees its cell to be opened again, though caches of its extents may
// hold its store still, to read them.
impl Drop for OpenCell {
    fn drop(&mut self) {
        // cladefs: closed already, as the daemon closes its stores (TASK-685.3).
        if self.closed {
            return;
        }
        // cladefs: the daemon closes every store it has, awaited ([`release`]): under madsim, a store
        // dropped open is one it let go of where it couldn't wait (doc-24, step 9); but for its
        // host's dying, killed, which closes nothing, as a process killed doesn't.
        if cfg!(madsim) && node_local::dying() {
            return;
        }
        if cfg!(madsim) && !std::thread::panicking() {
            panic!(
                "a store dropped open, {}: the daemon closes its stores (open_cell::release)",
                self.path.display()
            );
        }
        // cladefs: what the fork recorded, its base's profile, if the window is not over yet.
        if let Some((recorder, _)) = self.profiling.take()
            && let Err(err) = recorder.write()
        {
            log::warn!("cladefs: writing a boot profile: {err}");
        }
        // cladefs: its recording of what its guest read of its version, written behind it as its
        // evidence goes.
        drop(self.evidence.take());
        // cladefs: its uploader first, which closes its open segment, uploads what it made local
        // and stops (TASK-192); then the store. A follower's store is its writer's. Closing from
        // the first: what waits for the close, as a fork of a version it made does, waits for its
        // uploads too (TASK-644).
        if self.following.is_none() {
            self.store.closing();
            fail::fail_point!("open_cell::closing");
        }
        drop(self.uploader.take());
        // cladefs: dropped unclosed, outside the simulation, closed here, waiting on the dropping
        // thread, as its uploader's drop waits (TASK-685.17): what closes a store awaits its close
        // ([`release`]).
        if self.following.is_none()
            && let Err(err) = local_cell::wait_here(self.store.close())
        {
            log::error!("cladefs: closing an extent store: {err:#}");
        }
    }
}

impl OpenCell {
    /// cladefs: closes it as dropping it does, and returns once it is closed: its uploader's
    /// uploads done and stopped (`Uploader::close`), and the store closed, which may be opened
    /// again at once (TASK-685.3). Holds no thread while it waits for its uploader, as the daemon
    /// closes its stores; dropped unclosed, it blocks.
    pub async fn close(mut self) {
        // cladefs: its recording of what its guest read of its version, written first.
        if let Some(evidence) = self.evidence.take() {
            evidence.close().await;
        }
        let profiling = self.profiling.take();
        let following = self.following.is_some();
        local_cell::upload::blocking(|| {
            // cladefs: what the fork recorded, its base's profile, if the window is not over yet.
            if let Some((recorder, _)) = profiling
                && let Err(err) = recorder.write()
            {
                log::warn!("cladefs: writing a boot profile: {err}");
            }
            if !following {
                self.store.closing();
            }
        });
        if !following {
            fail::fail_point!("open_cell::closing");
        }
        if let Some(uploader) = self.uploader.take() {
            uploader.close().await;
        }
        if !following && let Err(err) = self.store.close().await {
            log::error!("cladefs: closing an extent store: {err:#}");
        }
        self.closed = true;
        // cladefs: then what it held open: a follower's writer, and the bases attached to it,
        // each closed if this was its last (doc-24, step 9).
        if let Some(following) = self.following.take() {
            release(following.writer).await;
        }
        if let Some(bases) = Arc::into_inner(std::mem::take(&mut self.bases)) {
            let by_cell = bases.by_cell.into_inner().unwrap_or_else(PoisonError::into_inner);
            for base in by_cell.into_values().flatten().filter_map(Arc::into_inner) {
                if let Some(store) = base.store.into_inner() {
                    release(store).await;
                }
            }
        }
    }

    /// The size of the blocks extents are read and written in.
    pub const BLOCK_SIZE: u32 = host_cell::BLOCK_SIZE;

    /// The most bytes an extent holds (doc-4's `max_extent`).
    pub const MAX_EXTENT: u64 = 64 << 20;

    /// cladefs: the most bytes of its own a read or a write holds as it copies, whatever its length
    /// (TASK-149.17): which a VM process names, so that a connection of the extent daemon's held
    /// a copy of each of its requests' whole lengths, as many as it serves at once, past what the
    /// protocol bounds of it.
    pub const PIECE: usize = 1 << 20;

    /// A new store at `path`, which must not exist, holding at most `capacity` bytes, with a cell
    /// of its own. cladefs: kept in this host's bucket, or, where this is no host of cladefs's system, a
    /// file bucket of its own, which its description names (`host_cell::create`, TASK-165.6),
    /// opened as this process opens stores ([`Opening::here`]).
    pub async fn create(path: impl AsRef<Path>, capacity: u64) -> Result<Self, Error> {
        Self::create_in(path, capacity, host_cell::DEFAULT_TENANT).await
    }

    /// cladefs: a new store at `path`, as [`OpenCell::create`] makes, of a new cell of `tenant`'s,
    /// its ID its bucket's to give (`bucket_cell::objectid`, TASK-364).
    pub async fn create_in(
        path: impl AsRef<Path>,
        capacity: u64,
        tenant: u64,
    ) -> Result<Self, Error> {
        Self::create_with(path, capacity, CellOf::New(tenant), Opening::here()?).await
    }

    /// cladefs: a new store at `path`, as [`OpenCell::create`] makes, of `cell`, which its
    /// bucket holds (`ENOENT` otherwise): made on a host without it, recovered as it opens
    /// (TASK-165.9).
    pub async fn create_for_cell(
        path: impl AsRef<Path>,
        capacity: u64,
        cell: CellId,
    ) -> Result<Self, Error> {
        Self::create_with(path, capacity, CellOf::Taken(cell), Opening::here()?).await
    }

    /// cladefs: a new store at `path`, of the cell `of` says, as [`OpenCell::create_in`] or
    /// [`OpenCell::create_for_cell`] makes it, opened as `opening` says
    /// ([`OpenCell::open_with`]): its cell's, an Fxfs one (TASK-364, doc-20). cladefs: in a span of its own, `store.create`
    /// (`bucket_cell::trace`, TASK-258).
    pub async fn create_with(
        path: impl AsRef<Path>,
        capacity: u64,
        of: CellOf,
        opening: Opening,
    ) -> Result<Self, Error> {
        Self::create_kind_with(path, capacity, (of, Kind::Fxfs), opening).await
    }

    /// cladefs: a new store at `path`, as [`OpenCell::create_with`] makes, a cell of `kind`,
    /// its cell (TASK-364): a SQLite one, which the sqlite_extents VFS alone opens, as a
    /// store a database is made in through the daemon is.
    pub async fn create_kind_with(
        path: impl AsRef<Path>,
        capacity: u64,
        (of, kind): (CellOf, Kind),
        opening: Opening,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        Self::creating(path, async |span| {
            let keeping = opening.keeping.clone();
            let opened = host_cell::create(path, capacity, of, kind, keeping).await?;
            span.set_cell(opened.store.description().cell);
            Self::new(path, opened, opening).await
        })
        .await
    }

    /// cladefs: `make`'s making of a new store at `path`, in a span of its own, `store.create`
    /// (`bucket_cell::trace`, TASK-258), which it names the store's cell in once it knows it:
    /// a store made, opened, or made closed ([`OpenCell::create_placed`]).
    async fn creating<T>(
        path: &Path,
        make: impl AsyncFnOnce(&trace::Span) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let span = trace::Span::start("store.create");
        span.set_with(trace::STORE, || path.display().to_string());
        span.run_async(make(&span)).await
    }

    /// cladefs: a new store at `path`, as [`OpenCell::create_with`] makes, of `cell`, kept in
    /// the bucket at `url`, its uploader as `keeping` says: in tests, a memory bucket, which this
    /// process's stores naming it share.
    #[cfg(test)]
    pub(crate) async fn create_kept_with(
        path: impl AsRef<Path>,
        capacity: u64,
        cell: CellId,
        url: &str,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        let kept = (Kind::Fxfs, Some(url), keeping.clone());
        let of = CellOf::Named(cell);
        let opened = host_cell::create_unmade(path, capacity, of, kept.0, kept.1, kept.2).await?;
        opened.made().map_err(Error::new)?;
        Self::new(path, opened, Opening { keeping, ..Opening::here()? }).await
    }

    /// cladefs: a new store at `path`, as [`OpenCell::create`] makes, of a new cell, kept in
    /// the bucket at `url`: in tests, a memory bucket, which this process's stores naming it share.
    #[cfg(test)]
    pub(crate) async fn create_kept_in(
        path: impl AsRef<Path>,
        capacity: u64,
        url: &str,
    ) -> Result<Self, Error> {
        let (path, opening) = (path.as_ref(), Opening::here()?);
        let (of, keeping) = (CellOf::New(0), opening.keeping.clone());
        let made = host_cell::create_unmade(path, capacity, of, Kind::Fxfs, Some(url), keeping);
        let opened = made.await?;
        opened.made().map_err(Error::new)?;
        Self::new(path, opened, opening).await
    }

    /// The store at `path`, as of its last commit. cladefs: opened as this process opens stores
    /// ([`Opening::here`]).
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with(path, Opening::here()?).await
    }

    /// cladefs: the store at `path`, as of its last commit, opened as `opening` says: its uploader
    /// takes its cell's head, with the token its placement names or `opening`'s, and recovers
    /// it, the store's write-ahead log put after its end (TASK-165.9); a log another writer's
    /// commits moved past refuses (`ESTALE`) until `cladefsctl take` drops it. A tenant's (TASK-43):
    /// the pages of its own extents, and of those it inherited from other cells but public ones
    /// ([`OpenCell::make_public`]), are of `opening`'s arena of physical memory, which it holds,
    /// and what reads into the extent cache for it, until they are gone (TASK-79.36); or of the
    /// default arena, without one. Those of public cells' extents, which every tenant's forks of
    /// them share, are of the default arena.
    ///
    /// cladefs: in a span of its own, `store.open` (`bucket_cell::trace`, TASK-258).
    pub async fn open_with(path: impl AsRef<Path>, opening: Opening) -> Result<Self, Error> {
        let path = path.as_ref();
        let span = trace::Span::start("store.open");
        span.set_with(trace::STORE, || path.display().to_string());
        let opened = async {
            // cladefs: an ephemeral cell's, with no uploader (doc-25, TASK-698.3).
            let store = match local_cell::upload::blocking(|| host_cell::is_ephemeral(path)) {
                // cladefs: given up to a live migration, it opens here no more (TASK-698.4).
                true if host_fs::exists(&path.join("migrating")) => return Err(Errno::EBUSY.into()),
                true => {
                    let (store, bucket, keys, forked) =
                        host_cell::open_ephemeral(path, &opening.keeping).await?;
                    let assembling =
                        Self::assemble(path, (store, bucket, None), Some(forked), opening);
                    let mut opened = assembling.await?;
                    opened.keys = Some(keys);
                    opened
                }
                false => {
                    let opened = host_cell::open(path, opening.keeping.clone()).await?;
                    Self::new(path, opened, opening).await?
                }
            };
            span.set_cell(store.cell());
            Ok(store)
        };
        span.run_async(opened).await
    }

    /// cladefs: the store at `path`, as [`OpenCell::open`] has it, a tenant's (TASK-43), its own
    /// extents' pages of `arena` ([`OpenCell::open_with`]).
    pub async fn open_in(path: impl AsRef<Path>, arena: Option<Arc<Arena>>) -> Result<Self, Error> {
        Self::open_with(path, Opening { arena, ..Opening::here()? }).await
    }

    /// cladefs: the store at `path`, a fork attached to other stores (backlog doc-10), to read
    /// through to: no guest boots from it, so that, as [`OpenCell::open_without_profile`] says,
    /// it reads no boot profile. Its pages are of `arena`, or the default arena if it is public,
    /// as a public base's are, which every tenant's VM processes hold read-only.
    pub async fn open_base(
        path: impl AsRef<Path>,
        arena: Option<Arc<Arena>>,
    ) -> Result<Self, Error> {
        Self::open_base_with(path, arena, Opening::here()?.keeping).await
    }

    /// cladefs: the store at `path`, a fork attached to other stores, as [`OpenCell::open_base`]
    /// opens it, its uploader as `keeping` says, as the extent daemon's.
    pub async fn open_base_with(
        path: impl AsRef<Path>,
        arena: Option<Arc<Arena>>,
        keeping: local_cell::upload::Options,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        let (public, cell) = local_cell::upload::blocking(|| -> Result<_, Error> {
            Ok((cell_dir::public_cells(path)?, local_cell::host_cell(path).map_err(Error::new)?))
        })?;
        let arena = arena.filter(|_| !public.contains(&cell.id));
        Self::open_with(path, Opening { arena, profiled: false, keeping }).await
    }

    /// cladefs: the store at `path`, as [`OpenCell::open`] has it, for work on it that boots no
    /// guest, as the extent daemon's forks, syncs and resizes of stores no VM has open: the boot
    /// profile of the base it is a fork of is neither read into the extent cache, which would put
    /// the pages of what it names, tenants' too, in the default arena, which every VM process
    /// holds, nor recorded (TASK-79.17).
    pub async fn open_without_profile(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with(path, Opening { profiled: false, ..Opening::here()? }).await
    }

    /// The store `opened` at `path`, opened as `opening` says, of whose cell and those it was
    /// forked from the cells `public` are public ([`cell_dir::public_cells`]).
    async fn new(path: &Path, opened: host_cell::Opened, opening: Opening) -> Result<Self, Error> {
        let host_cell::Opened { store, bucket, uploader } = opened;
        let forked = uploader.forked_from();
        Self::assemble(path, (store, bucket, Some(uploader)), forked, opening).await
    }

    /// cladefs: the store `store` at `path`, its cell's extents read in `bucket`, with its uploader
    /// if it has one (an ephemeral cell's has none, doc-25), a fork of the version `forked` says,
    /// if any, opened as `opening` says: as [`OpenCell::new`] has it.
    async fn assemble(
        path: &Path,
        (store, bucket, uploader): (
            Arc<LocalCell<HostDir>>,
            Arc<bucket_cell::object_bucket::ObjectBucket>,
            Option<local_cell::upload::Uploader<HostDir>>,
        ),
        forked: Option<local_cell::upload::Forked>,
        opening: Opening,
    ) -> Result<Self, Error> {
        // cladefs: what may fail of it first, its store closed if it does (TASK-685.3).
        let made = local_cell::upload::blocking(|| -> Result<_, Error> {
            cache::relieve_pressure();
            let public = cell_dir::public_cells(path)?;
            let forked_from = Arc::new(cell_dir::ancestry(path)?);
            let cache = opening.keeping.cache.clone();
            let fetcher = crate::bucket::HostFetcher::new(bucket.clone(), cache)?;
            Ok((public, forked_from, fetcher))
        });
        let (public, forked_from, fetcher) = match made {
            Ok(made) => made,
            Err(err) => {
                drop((store, bucket));
                if let Some(uploader) = uploader {
                    uploader.close().await;
                }
                return Err(err);
            }
        };
        let Opening { arena, profiled, keeping: _ } = opening;
        let arenas = Arenas { own: arena, public: Arc::new(public) };
        let profiling = profiled.then(|| Self::profile(&store, &arenas, &forked_from, &fetcher));
        let cell = store.description().cell;
        // cladefs: a guest's boot of a fork of a version, whose reads its version's evidence
        // predicts, and records (TASK-702.2).
        let evidence = (forked.filter(|_| profiled)).map(|forked| {
            crate::evidence::Evidence::opened(forked, cell, bucket.clone(), fetcher.runtime())
        });
        Ok(Self {
            profiling: profiling.flatten(),
            evidence,
            arenas,
            forked_from,
            store,
            bucket,
            path: path.to_owned(),
            bases: Default::default(),
            dax: CellMutex::new("dax", None),
            uploader,
            keys: None,
            opening: StoreRwLock::new("opening", ()),
            following: None,
            committed: event_listener::Event::new(),
            changes: CellMutex::new("changes", WindowChanges::default()),
            flush_turns: Default::default(),
            halt: Halt::new(path, cell),
            fetcher,
            closed: false,
        })
    }

    /// cladefs: a follower of the store (backlog doc-8): a store of its own, read-only, with a window
    /// of its own, which reads the store, and maps its extents' cached pages, as of its latest
    /// commit, through a view of it, while the store's writer goes on. Anything outside its
    /// view's commits and its pins it refuses with `EIO`, logged; what would change the store,
    /// with `EROFS`. Its view moves to the writer's latest commit as it advances.
    pub fn follow(self: &Arc<Self>) -> OpenCell {
        let view = CellMutex::new("view", self.store.view());
        let following = Following { writer: self.clone(), view };
        Self {
            profiling: None,
            evidence: None,
            arenas: self.arenas.clone(),
            forked_from: self.forked_from.clone(),
            store: self.store.clone(),
            bucket: self.bucket.clone(),
            path: self.path.clone(),
            bases: self.bases.clone(),
            dax: CellMutex::new("dax", None),
            uploader: None,
            keys: None,
            opening: StoreRwLock::new("opening", ()),
            following: Some(following),
            committed: event_listener::Event::new(),
            changes: CellMutex::new("changes", WindowChanges::default()),
            flush_turns: Default::default(),
            halt: Halt::new(&self.path, self.cell()),
            fetcher: self.fetcher.clone(),
            closed: false,
        }
    }

    /// cladefs: what it is, which says what may be done to it ([`CellAccess`]).
    pub fn access(&self) -> CellAccess {
        let role = match &self.following {
            Some(_) => CellRole::Follower,
            None if self
                .uploader
                .as_ref()
                .is_some_and(local_cell::upload::Uploader::is_at_version) =>
            {
                CellRole::Version
            }
            None => CellRole::Writer,
        };
        CellAccess { role, public: self.is_public() }
    }

    /// cladefs: halts the store, as `why` says, unless it is already: a panic went through it past
    /// its locks, as one serving a request of it, which a panic holding one of them does itself.
    /// It serves nothing more ([`crate::halt`], TASK-153.16).
    // The extent daemon's (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn halt(&self, why: impl FnOnce() -> String) {
        self.halt.halt(why);
    }

    /// cladefs: whether the store is halted ([`OpenCell::halt`]): its requests fail with `EIO`,
    /// or `EACCES` once its host forgot its tenant's or cell's keys (`crate::bucket::forget`).
    // The extent daemon's (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn is_halted(&self) -> bool {
        self.halt.is_halted()
    }

    /// cladefs: the errno the store's requests fail with, if it is halted ([`OpenCell::is_halted`]).
    // The extent daemon's (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn halted(&self) -> Option<Errno> {
        self.halt.errno()
    }

    /// cladefs: makes the store a new writer's, which its followers go on following: one opened for
    /// followers alone, or a writer's that went. `EBUSY` unless nothing is open in it
    /// ([`OpenCell::is_quiescent`]): what a writer that went had written since its last commit
    /// its new writer's filesystem doesn't know of. The window goes, which was the writer's that
    /// went: the new one creates its own.
    pub fn take_over(&self) -> Result<()> {
        self.admit(Changing::Contents)?;
        if !self.is_quiescent()? {
            return Err(Errno::EBUSY);
        }
        let window = self.dax.lock(&self.halt)?.take();
        drop(window);
        // cladefs: its uploader goes on, its going no writer's to wait for (TASK-216).
        if let Some(uploader) = &self.uploader {
            uploader.drain_on_stop();
        }
        Ok(())
    }

    /// cladefs: what has its uploader, once it stops, stop at once, uploading nothing more, which
    /// its write-ahead log keeps for its next opening (TASK-216): what cladefsd has a writer that
    /// waits for its going connection's store to close call. None for a store with no uploader.
    pub fn cut_short(&self) -> Option<crate::cell_table::Wanted> {
        let uploader = self.uploader.as_ref()?;
        Some(std::sync::Arc::new(uploader.cut_short()))
    }

    /// cladefs: cuts the store's followers off: their reads, advances and pins fail with `ESTALE`
    /// from now on, until they follow it anew. What its writer's going with writes uncommitted
    /// does: the store is no new writer's ([`OpenCell::take_over`]) while they follow it.
    pub fn cut_followers(&self) {
        self.store.cut_views();
    }

    /// cladefs: a follower's view, or `EINVAL` if it is a writer's store.
    fn view(&self) -> Result<CellGuard<'_, MutexGuard<'_, local_cell::View>>> {
        let following = self.following.as_ref().ok_or(Errno::EINVAL)?;
        following.view.lock(&self.halt)
    }

    /// The number of the store's latest commit: its writer's, if a follower's.
    pub fn committed(&self) -> Result<u64> {
        self.store.last_commit().map_err(errno)
    }

    /// cladefs: the number of the commit a follower's view is as of.
    pub fn view_commit(&self) -> Result<u64> {
        Ok(self.view()?.commit())
    }

    /// cladefs: moves a follower's view to the writer's latest commit, holding what the commits
    /// since the one it was as of held, until it lets go of them ([`OpenCell::let_go`]): what
    /// its guest's follower reads as it applies them. Returns the commit's number.
    pub fn advance(&self) -> Result<u64> {
        let mut view = self.view()?;
        view.advance_keeping().map_err(errno)?;
        Ok(view.commit())
    }

    /// cladefs: lets go of what a follower's view held of the commits before the one
    /// numbered `commit`, at most its own: their frees go, but for its pins. One of an earlier
    /// commit than it holds since, coming after its next advance, changes nothing.
    pub fn let_go(&self, commit: u64) -> Result<()> {
        self.view()?.let_go(commit);
        Ok(())
    }

    /// cladefs: keeps `extent`, which a follower's view holds, whatever commit it advances to,
    /// until unpinned: what its guest holds open that the writer deletes.
    pub fn pin(&self, extent: ExtentId) -> Result<()> {
        // cladefs: nothing the writer does takes a base's extents away.
        if self.bases.routes(extent.cell) {
            return Ok(());
        }
        // cladefs: one its view holds, as doc-4 has it, not any ID a VM process makes up, each of
        // which the daemon would hold for as long as the view (TASK-588).
        let view = self.view()?;
        self.store.view_extent(&view, extent).map_err(errno)?;
        view.pin(extent).map_err(errno)
    }

    /// cladefs: lets go of `extent`, pinned.
    pub fn unpin(&self, extent: ExtentId) -> Result<()> {
        self.view()?.unpin(extent);
        Ok(())
    }

    /// cladefs: waits at most `timeout` for a commit of the store after the one numbered
    /// `commit`, returning the latest commit's number, if one came: what tells its followers.
    pub fn newer(&self, commit: u64, timeout: std::time::Duration) -> Option<u64> {
        self.store.newer(commit, timeout)
    }

    /// cladefs: what says when the store's writer next commits ([`OpenCell::newer`] then has it),
    /// through the writer's store, if a follower's: a future, which holds no thread.
    pub fn commits(&self) -> event_listener::EventListener {
        match &self.following {
            Some(following) => following.writer.committed.listen(),
            None => self.committed.listen(),
        }
    }

    /// cladefs: what the store's followers are (TASK-42.7): their views, how far behind, found
    /// without the store's state (TASK-92.17).
    pub fn view_stats(&self) -> local_cell::ViewStats {
        self.store.view_stats()
    }

    /// cladefs: the bytes of frees the store's followers' views keep, which count as used
    /// (TASK-42.7).
    pub fn kept_for_views(&self) -> Result<u64> {
        self.store.kept_for_views().map_err(errno)
    }

    /// cladefs: what reads the extents the store holds whose data is in the bucket, at their
    /// locations there, keeping what it fetches in the host's cache, if its uploader has one
    /// (TASK-165.8): a follower's, its writer's.
    fn fetcher(&self) -> Arc<crate::bucket::HostFetcher> {
        self.fetcher.clone()
    }

    /// cladefs: for a fork of a base, reads the base's boot profile into the extent cache, behind
    /// the fork, if there is one; or records the fork's page-ins as the base's profile, and writes
    /// it once the window is over.
    fn profile(
        store: &Arc<LocalCell<HostDir>>,
        arenas: &Arenas,
        forked_from: &[(u64, std::path::PathBuf)],
        fetcher: &Arc<crate::bucket::HostFetcher>,
    ) -> Option<(Arc<Recorder>, tokio::sync::oneshot::Sender<()>)> {
        let (base, dir) = forked_from.first()?;
        // A base deleted since has no profile, and records none.
        if !host_fs::exists(&dir) {
            return None;
        }
        let path = profile::path(dir, *base);
        match profile::read(&path) {
            Ok(units) => {
                replay(store.clone(), arenas.clone(), fetcher.clone(), units);
                return None;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                log::warn!("cladefs: reading the boot profile at {}: {err}", path.display());
                return None;
            }
        }
        // One fork of the base records it, of those that start before it is written: not a
        // recorder and a thread each of a few hundred starting at once (TASK-34).
        static RECORDING: Mutex<std::collections::BTreeSet<std::path::PathBuf>> =
            Mutex::new(std::collections::BTreeSet::new());
        if !RECORDING.lock().unwrap().insert(path.clone()) {
            return None;
        }
        let recorder = Arc::new(Recorder::new(path.clone(), store.description().cell.id));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let writer = recorder.clone();
        // cladefs: a task, its write off the runtime's threads (doc-24, step 5).
        drop(fetcher.spawn(async move {
            // At the window's end, or the store's closing, which writes it itself.
            if tokio::time::timeout(profile::WINDOW, stopped).await.is_err()
                && let Err(err) = local_cell::upload::blocking(|| writer.write())
            {
                log::warn!("cladefs: writing a boot profile: {err}");
            }
            RECORDING.lock().unwrap().remove(&path);
        }));
        Some((recorder, stop))
    }

    /// cladefs: forks the store, as of its last commit, made uploaded first, and returns the fork's
    /// cell: a new cell of `cell`, its ID the bucket's to give (`bucket_cell::objectid`,
    /// TASK-364), holding what it holds, whose store is a new directory at `path`, which
    /// [`OpenCell::open`] opens: `EEXIST` if anything is at `path`. The fork is made in the
    /// bucket alone, by the store's uploader, which keeps its cell in the bucket its
    /// description names: a commit naming the child, whose acknowledgement gives the child its
    /// record, inheriting the store's extents rather than copies (doc-7, Forks). The child's
    /// directory is made then, its store recovering the child from the bucket as it opens, its
    /// extents read from there, or the host's cache (`host_cell::fork`, TASK-165.7, TASK-165.9).
    /// A fork that fails is deleted in the bucket, as the fork's alone, and leaves nothing at
    /// `path`.
    ///
    /// A fork of a version commits nothing in object storage: its child's record is the fork, which
    /// fails with `ENOENT` once the version is deleted (`Uploader::forked`).
    ///
    /// cladefs: in a span of its own, `store.fork` (`bucket_cell::trace`, TASK-258).
    pub async fn fork(&self, path: impl AsRef<Path>, tenant: u64) -> Result<CellId, Error> {
        let cell = self.new_child(tenant).await?;
        self.fork_to(path, cell).await?;
        Ok(cell)
    }

    /// cladefs: forks the store, at a version, to a new cell of `tenant`'s, an ephemeral cell kept on
    /// this host alone, whose store is made at `path` (doc-25, TASK-698.3;
    /// `host_cell::fork_ephemeral`), and returns its cell: `EINVAL` of a store at no version,
    /// `ENOTSUP` on a host keeping no ephemeral cells.
    pub async fn fork_ephemeral(
        &self,
        path: impl AsRef<Path>,
        tenant: u64,
    ) -> Result<CellId, Error> {
        let uploader = self.uploader.as_ref().ok_or(Errno::EROFS)?;
        if !uploader.is_at_version() {
            return Err(Errno::EINVAL.into());
        }
        let cell = self.new_child(tenant).await?;
        let child = bucket_cell::layout::Child { cell, owner: rand::random() };
        let inherited = |to: &Path| -> anyhow::Result<()> {
            Ok(cell_dir::inherit(&self.path, self.cell().id, &self.forked_from, to)?)
        };
        let forking =
            host_cell::fork_ephemeral(&self.store, uploader, path.as_ref(), child, inherited);
        self.fork_span(cell).run_async(forking).await?;
        Ok(cell)
    }

    /// cladefs: a fork's child, the cell of a new cell of `tenant`'s, its ID the store's bucket's
    /// to give. A follower's uploader
    /// is its writer's, which forks nothing of it (`EROFS`). A store with a writer forks to no cell
    /// of another tenant's than its own unless it is the default tenant's (`EACCES`,
    /// `host_cell::may_fork_into`, TASK-395.9): refused before the bucket gives an ID.
    async fn new_child(&self, tenant: u64) -> Result<CellId, Error> {
        self.admit(Changing::Cell)?;
        let uploader = self.uploader.as_ref().ok_or(Errno::EROFS)?;
        host_cell::may_fork_into(self.cell(), uploader, tenant)?;
        Ok(host_cell::new_cell(uploader.bucket(), tenant).await.map_err(upload_errno)?)
    }

    /// cladefs: forks the store as [`OpenCell::fork`] does, to `cell`, whatever its ID: so the
    /// tests make a fork whose child's ID another cell took, which IDs given out by the bucket
    /// never are, to show a fork refuses it (`EEXIST`) and harms no other cell (TASK-79.82).
    pub(crate) async fn fork_to(&self, path: impl AsRef<Path>, cell: CellId) -> Result<(), Error> {
        let forking = async {
            self.admit(Changing::Cell)?;
            self.fork_at_version(path.as_ref(), cell, false).await
        };
        self.fork_span(cell).run_async(forking).await
    }

    /// cladefs: the span of a fork of it to `cell`.
    fn fork_span(&self, cell: CellId) -> trace::Span {
        let span = trace::Span::start("store.fork");
        span.set_cell(self.cell());
        span.set_with(trace::CHILD, || cell.to_string());
        span
    }

    /// cladefs: forks the store, as [`OpenCell::fork`] says, to `cell` at `path`: a store at a
    /// version forks itself; any other, a version of its last commit, made (of it quiescent, if
    /// `quiescent`) and uploaded, a store at which, beside `path`, forks, then dropped, its child
    /// keeping what it inherits by its reference. Forks are of versions alone (TASK-508.3).
    async fn fork_at_version(
        &self,
        path: &Path,
        cell: CellId,
        quiescent: bool,
    ) -> Result<(), Error> {
        // A follower's uploader is its writer's, which forks nothing of it.
        let uploader = self.uploader.as_ref().ok_or(Errno::EROFS)?;
        // cladefs: the child's owner, which every record of it in object storage names, whoever
        // writes it, so that another fork to its ID, whose owner differs, is told apart
        // (TASK-79.82, TASK-153.9).
        let child = bucket_cell::layout::Child { cell, owner: rand::random() };
        // cladefs: what is attached to it stays attached to the fork, a restored VM's too; and what
        // is public of it, and what it was forked from.
        let inherited = |to: &Path| -> anyhow::Result<()> {
            Ok(cell_dir::inherit(&self.path, self.cell().id, &self.forked_from, to)?)
        };
        // Refused before anything is made, the version among it.
        host_cell::may_fork_into(self.cell(), uploader, cell.tenant)?;
        // cladefs: at rest, if `quiescent`, until its version is committed, no write coming
        // between: what follows forks the version, its writes after it its own (TASK-685.3).
        let id = match (uploader.is_at_version(), quiescent) {
            (true, quiescent) => {
                if quiescent {
                    self.at_rest(async { Ok(()) }).await?;
                }
                return host_cell::fork(&self.store, uploader, path, child, inherited).await;
            }
            (false, true) => self.at_rest(self.commit_version(None, true)).await?,
            (false, false) => self.commit_version(None, false).await?,
        };
        self.make_uploaded().await?;
        failpoint!("fork::uploaded");
        host_cell::fork_through(&self.store, uploader, id, path, child, inherited).await
    }

    /// cladefs: takes `cell` into the store at `path`, of `capacity` bytes until a commit of the
    /// cell's sets one, placed at `token`'s placement if given (doc-14, Opening a store): a new
    /// one if nothing is at `path`, which recovers the cell as the bucket holds it; or the one
    /// there, opened, its write-ahead log dropped if another writer's commits moved the cell
    /// past it (`ESTALE` opening it otherwise), the commits it held, never acknowledged, lost
    /// (doc-7, Recovery on the same host; TASK-165.9). Returns once it holds the cell, closed.
    pub async fn take(
        path: impl AsRef<Path>,
        capacity: u64,
        cell: CellId,
        token: Option<u64>,
    ) -> Result<(), Error> {
        Self::take_with(path, (capacity, cell, token), Opening::here()?).await?.close().await;
        Ok(())
    }

    /// cladefs: takes `cell` into the store at `path` as [`OpenCell::take`] does, of `capacity`
    /// bytes, placed at `token`'s placement if given, and returns it, opened as `opening` says.
    pub async fn take_with(
        path: impl AsRef<Path>,
        (capacity, cell, token): (u64, CellId, Option<u64>),
        opening: Opening,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        // cladefs: a store made here is opened once, as made, not made, closed and opened again
        // (TASK-449).
        let opened = if !host_fs::exists(&path) {
            let kept = opening.keeping.clone();
            let of = CellOf::Taken(cell);
            Self::made_placed(path, (capacity, of, token), Kind::Fxfs, kept).await?
        } else if local_cell::host_cell(path).map_err(Error::new)? != cell {
            let refused = format!("{} holds another cell than {cell}", path.display());
            return Err(Error::new(anyhow::Error::new(Errno::EEXIST).context(refused)));
        } else {
            if let Some(token) = token {
                local_cell::upload::blocking(|| Self::place(path, token))?;
            }
            host_cell::take(path, opening.keeping.clone()).await?
        };
        Self::new(path, opened, opening).await
    }

    /// cladefs: makes a new store at `path`, which must not exist, of `capacity` bytes and the cell
    /// `of` says, a cell of `kind` (TASK-364), its uploader as `keeping`
    /// says, placed at `token`'s placement if given, closes it, and returns its cell: what
    /// [`OpenCell::take_with`] takes a cell into, and the daemon's `CreateCell` makes. Its
    /// cell, and so its head, is taken as it is made with the token of the placement it is given,
    /// not `keeping`'s, this host's: a new copy placed later
    /// took no cell from an earlier placement (`ESTALE`), the head taken before the placement
    /// was recorded (TASK-149.20, TASK-266).
    pub async fn create_placed(
        path: impl AsRef<Path>,
        (capacity, of, token): (u64, CellOf, Option<u64>),
        kind: Kind,
        keeping: local_cell::upload::Options,
    ) -> Result<CellId, Error> {
        let opened = Self::made_placed(path.as_ref(), (capacity, of, token), kind, keeping).await?;
        let cell = opened.store.description().cell;
        opened.close().await;
        Ok(cell)
    }

    /// cladefs: makes a new store at `path` as [`OpenCell::create_placed`] does, and returns it
    /// opened, as made: what [`OpenCell::take_with`] opens.
    async fn made_placed(
        path: &Path,
        (capacity, of, token): (u64, CellOf, Option<u64>),
        kind: Kind,
        keeping: local_cell::upload::Options,
    ) -> Result<host_cell::Opened, Error> {
        let keeping =
            local_cell::upload::Options { token: token.unwrap_or(keeping.token), ..keeping };
        let force = keeping.force;
        let opened = Self::creating(path, async |span| {
            let opened = host_cell::create(path, capacity, of, kind, keeping).await?;
            span.set_cell(opened.store.description().cell);
            Ok(opened)
        })
        .await?;
        // cladefs: a forced taking's placement forced from then on (TASK-681).
        let placed = local_cell::upload::blocking(|| match (token, force) {
            (Some(token), true) => Self::place_forced(path, token),
            (Some(token), false) => Self::place(path, token),
            (None, _) => Ok(()),
        });
        if let Err(err) = placed {
            opened.close().await;
            return Err(err);
        }
        Ok(opened)
    }

    /// cladefs: places the store at `path` at the placement
    /// whose fencing token, its boot ID, is `token` (TASK-39.7): its host takes its cell's
    /// cell with that token from then on, and its writer the cell's head with that taking's
    /// epoch; a later placement's token exceeds it, so a placement cut off takes no cell back
    /// from the one after it (`ESTALE`), and its commits fail once a later one takes it (`EROFS`).
    pub fn place(path: impl AsRef<Path>, token: u64) -> Result<(), Error> {
        local_cell::place_host(path.as_ref(), token, false).map_err(Error::new)
    }

    /// cladefs: places the store at `path` as [`OpenCell::place`] does, its placement's takings
    /// of a replicated cell forced (doc-21, TASK-681): a taking goes on with the members of the
    /// configuration the cell's head names that answer, fewer than a take needs, as when f+1 of
    /// them are gone for good. Commits told replicated that only the others held are lost.
    pub fn place_forced(path: impl AsRef<Path>, token: u64) -> Result<(), Error> {
        local_cell::place_host(path.as_ref(), token, true).map_err(Error::new)
    }

    /// cladefs: deletes the store at `path`, which nothing has open: with `bucket`, its cell
    /// there first, its forks' cells living on, which the collector keeps what they inherited
    /// for; then its directory, which holds its cell alone (TASK-165.7). A deletion that stopped
    /// partway is finished by deleting again. Fails with `ENOENT` if there is none at `path`, and
    /// with `ESTALE` if a later placement holds its cell in `bucket`.
    pub async fn delete(
        path: impl AsRef<Path>,
        bucket: Option<&bucket_cell::object_bucket::ObjectBucket>,
    ) -> Result<(), Error> {
        host_cell::delete(path.as_ref(), bucket, crate::bucket::host_token).await
    }

    /// cladefs: the bucket the description of the store at `path` names, which its cell is kept
    /// in, if its description is there: what deleting it deletes its cell in. A store of another
    /// format is refused (`local_cell::Format`).
    pub fn bucket_at(
        path: impl AsRef<Path>,
    ) -> Result<Option<Arc<bucket_cell::object_bucket::ObjectBucket>>, Error> {
        host_cell::bucket_at(path.as_ref()).map_err(Error::new)
    }

    /// cladefs: the bucket the store's description names, which its cell is kept in: what its
    /// daemon attaches it to (TASK-165.6).
    pub fn bucket(&self) -> &Arc<bucket_cell::object_bucket::ObjectBucket> {
        &self.bucket
    }

    /// cladefs: the store's uploader, if it has one.
    #[cfg(test)]
    pub(crate) fn uploader(&self) -> Option<&local_cell::upload::Uploader<HostDir>> {
        self.uploader.as_ref()
    }

    /// cladefs: whether its store's map of the extents the bucket holds of its cell is `other`'s,
    /// one map shared (`local_cell::LocalCell::shares_extents_with`, TASK-453).
    #[cfg(test)]
    pub(crate) fn shares_extents_with(&self, other: &Self) -> bool {
        self.store.shares_extents_with(&other.store).unwrap()
    }

    /// cladefs: whether what its guest reads now is recorded, as its version's read evidence
    /// ([`OpenCell::guest_read`], TASK-702.2).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn records_reads(&self) -> bool {
        self.evidence.as_ref().is_some_and(crate::evidence::Evidence::records)
    }

    /// cladefs: says its guest read `len` bytes at `offset` in `extent`, begun at `began`, by a
    /// `READ`, a `MAP` or a fault, which its version's read evidence records, if it records
    /// ([`OpenCell::records_reads`], TASK-702.2).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn guest_read(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
        began: std::time::Instant,
    ) {
        if let Some(evidence) = &self.evidence {
            evidence.read(extent, offset, len, began);
        }
    }

    /// cladefs: of a fork of a version, the version and its keys, as its uploader says them
    /// (`Uploader::forked_from`): for tests of its read evidence.
    #[cfg(test)]
    pub(crate) fn forked_from(&self) -> Option<local_cell::upload::Forked> {
        self.uploader.as_ref()?.forked_from()
    }

    /// cladefs: of a store opened for a guest, a fork of a version, what its version's read evidence
    /// predicts it reads (TASK-702.2).
    #[cfg_attr(not(test), expect(dead_code, reason = "TASK-702.3 prefetches what it predicts"))]
    pub(crate) fn predicted(&self) -> Option<crate::evidence::Predicted> {
        self.evidence.as_ref().map(crate::evidence::Evidence::predicted)
    }

    /// cladefs: `extent`'s length, and whether it is open, if its store holds it, as reads ask
    /// (`local_cell::LocalCell::extent`); and as traces of reads name it (TASK-702.1).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn stored_extent(&self, extent: ExtentId) -> Result<Option<(u64, bool)>> {
        self.store.extent(extent).map_err(errno)
    }

    /// cladefs: a read's of `extent` from the extent cache ([`OpenCell::read_cached`]).
    #[cfg(test)]
    pub(crate) async fn read_from_cache(
        &self,
        extent: ExtentId,
        offset: u64,
        data: &mut [u8],
    ) -> Result<bool> {
        self.read_cached(extent, offset, data).await
    }

    /// The cell whose extents the guest mints, by its ID, in its cell.
    pub fn cell(&self) -> CellId {
        self.store.description().cell
    }

    /// cladefs: the bytes the cell's files take of the host's disk, as its store's footprint counts
    /// them: what is not acknowledged in the bucket yet, and the open segment (doc-22).
    pub fn disk_bytes(&self) -> u64 {
        self.store.dir().footprint().bytes()
    }

    /// What the store holds (`INFO`).
    pub fn info(&self) -> Result<ExtentInfo> {
        let info = self.store.info().map_err(errno)?;
        // cladefs: from its writer's range, with an uploader (doc-7, The head).
        let floor = self.uploader.as_ref().map_or(0, local_cell::upload::Uploader::next_sequence);
        Ok(ExtentInfo {
            capacity: info.capacity,
            used: info.used,
            available: info.available,
            next_sequence: info.next_sequence,
            sequence_floor: floor,
        })
    }

    /// cladefs: what its cell holds, the bytes (TASK-49) and the number of its extents (TASK-454),
    /// and whose: its cell's own, or inherited. Kept on as they change, not counted now: as cheap
    /// as [`OpenCell::info`], however many extents it holds.
    pub fn holding(&self) -> Result<CellHolding> {
        let info = self.store.info().map_err(errno)?;
        Ok(CellHolding {
            bytes: Whose { own: info.own, inherited: info.inherited },
            extents: Whose { own: info.own_extents, inherited: info.inherited_extents },
        })
    }

    /// Sets the most bytes the store's extents may hold, in a commit of its own: grows it, or
    /// shrinks it no lower than what its extents hold (`ENOSPC`). The guest's filesystem, which may
    /// hold more, agrees to a shrink first (the extent daemon's `RESIZE` asks it).
    pub async fn set_capacity(&self, capacity: u64) -> Result<()> {
        self.admit(Changing::Capacity)?;
        self.store.set_capacity(capacity).await.map_err(errno)
    }

    /// Reads the bytes at `offset` in `extent` into `buffers`, in order (`READ`). Blocks never
    /// written read as zeros. cladefs: what it reads in, from the bucket, a base opened, or the
    /// cache's page-ins, awaited, holding no thread (doc-24, step 5).
    pub async fn read(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &mut [VolatileSliceMut<'_>],
    ) -> Result<()> {
        let len = buffers.iter().map(VolatileSliceMut::len).sum::<usize>();
        stats::add(Counter::Reads, 1);
        stats::add(Counter::ReadBytes, len as u64);
        // cladefs: nothing a panic went through is read (`crate::halt`).
        self.halt.check()?;
        let base = self.base_holding(extent).await?;
        // cladefs: a piece at a time, as the extent daemon's memory holds no more of a request
        // (TASK-149.17); once at least, as a read of nothing finds whether the extent is there.
        let mut piece = vec![0; len.min(Self::PIECE)];
        let mut done = 0;
        loop {
            let data = &mut piece[..(len - done).min(Self::PIECE)];
            let at = offset.checked_add(done as u64).ok_or(Errno::EINVAL)?;
            let (from, through_base) = match &base {
                Some((base, _)) => (&**base, true),
                None => (self, false),
            };
            if !from.read_cached(extent, at, data).await? {
                from.read_stored(extent, at, data).await?;
            }
            let _ = through_base;
            scatter(buffers, done, data);
            done += data.len();
            if done == len {
                return Ok(());
            }
        }
    }

    /// cladefs: reads `extent`'s bytes at `offset` from the extent cache, if it is sealed: which pages
    /// them in from the store, or object storage, unless they are in memory already, read by any
    /// store or fork before (TASK-32). Returns whether it read them: not a follower's, whose view
    /// decides what it holds, nor an open extent's, nor one the store doesn't hold.
    async fn read_cached(&self, extent: ExtentId, offset: u64, data: &mut [u8]) -> Result<bool> {
        if self.following.is_some() {
            return Ok(false);
        }
        let Some((len, false)) = self.store.extent(extent).map_err(errno)? else {
            return Ok(false);
        };
        let end = offset.checked_add(data.len() as u64).ok_or(Errno::EINVAL)?;
        if end > Self::MAX_EXTENT {
            return Err(Errno::EINVAL);
        }
        // Past its end, zeros.
        let held = len.saturating_sub(offset).min(data.len() as u64) as usize;
        let (bytes, zeros) = data.split_at_mut(held);
        zeros.fill(0);
        if bytes.is_empty() {
            return Ok(true);
        }
        let cached = self.cached(extent, len).await?;
        match cached.read(offset, bytes).await {
            Ok(()) => Ok(true),
            // Freed meanwhile, or its page-in failed: as the store says.
            Err(errno) => {
                log::debug!("cladefs: reading {extent:?} at {offset} from the cache: {errno:?}");
                Ok(false)
            }
        }
    }

    /// Reads a sealed extent's bytes from the store, or, cladefs, from object storage, if the store
    /// holds it without its data.
    async fn read_stored(&self, extent: ExtentId, offset: u64, data: &mut [u8]) -> Result<()> {
        stats::add(Counter::ReadStoredBytes, data.len() as u64);
        let read = match &self.following {
            Some(following) => {
                let view = following.view.lock(&self.halt)?;
                self.store.read_view(&view, extent, offset, data)
            }
            None => self.store.read(extent, offset, data),
        };
        match read {
            Err(ref error)
                if let Some(in_bucket) = error.downcast_ref::<local_cell::BucketData>() =>
            {
                let (location, len) = (in_bucket.location.clone(), data.len() as u64);
                let fetched = self.fetcher().read(extent, location, offset, len).await;
                let fetched = fetched.map_err(|failure| {
                    match failure {
                        // cladefs: a follower's of an extent its writer freed since its view's
                        // commit, which the bucket's collector reclaimed: it is cut off, as one too
                        // far behind its writer is (doc-8, decision 7; TASK-165.8).
                        local_cell::upload::Failure::Bucket(bucket_cell::Error::Lost(ref lost))
                            if self.following.is_some() =>
                        {
                            log::info!("cladefs: a follower's {extent:?} is gone: {lost}");
                            Errno::ESTALE
                        }
                        failure => upload_errno(failure),
                    }
                })?;
                data[..fetched.len()].copy_from_slice(&fetched);
                data[fetched.len()..].fill(0);
                Ok(())
            }
            read => read.map_err(errno),
        }
    }

    /// Writes as [`write`](Self::write) does, ordered with `barriers` (`WRITE`'s flags): one
    /// request, where each is a round trip from the guest.
    pub async fn write_with_barriers(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
        barriers: WriteBarriers<'_>,
    ) -> Result<()> {
        // A root is set by the commit after the write: there is none to set it.
        if barriers.root.is_some() && barriers.after.is_none() {
            return Err(Errno::EINVAL);
        }
        if barriers.before {
            self.commit_ordered(None).await?;
        }
        self.write(extent, offset, buffers).await?;
        match barriers.after {
            None => Ok(()),
            Some(commit) => self.commit_with(barriers.root, commit).await,
        }
    }

    /// Writes the bytes of `buffers`, in order, at `offset` in `extent`, which must be the store's
    /// cell's, and open (`WRITE`).
    pub async fn write(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
    ) -> Result<()> {
        self.admit(Changing::Contents)?;
        let len = buffers.iter().map(VolatileSlice::len).sum::<usize>();
        let _opening = self.opening.read(&self.halt).await?;
        // cladefs: from `buffers` to the store's file, a piece at a time, as the extent daemon's
        // memory holds no more of a request (TASK-149.17).
        let fill = |at: u64, piece: &mut [u8]| {
            gather(buffers, at as usize, piece);
        };
        let written = self.store.write_with(extent, offset, len as u64, Self::PIECE, fill);
        written.await.map_err(errno)
    }

    /// Frees `extent`, if the store holds it (`FREE`).
    pub fn free(&self, extent: ExtentId) -> Result<()> {
        self.admit(Changing::Contents)?;
        self.store.free(extent).map_err(errno)?;
        // cladefs: its pages, unless another store that asked for them holds it still, which reads
        // them through itself (TASK-79.7): one that holds it reads them anew.
        let Some(cached) = cache::freed(&self.store, self.arenas.of(extent), extent) else {
            return Ok(());
        };
        // cladefs: the window's mappings of it, which kept its pages, map zeros instead: a guest
        // rewriting the same blocks, sealing each version (a database's log, fsynced at each
        // transaction), filled the cache with pages only the window's mappings kept, which no
        // eviction could take (ENOMEM).
        let mut dax = self.dax.lock(&self.halt)?;
        let Some(dax) = dax.as_mut() else { return Ok(()) };
        let freed = dax.map_zeros_over(&cached);
        let mut changes = self.changes.lock(&self.halt)?;
        for range in freed {
            changes.record(range.start, range.end - range.start, Change::Freed);
        }
        Ok(())
    }

    /// Commits what was written and freed, and `root` as the store's root if given, uploaded
    /// (`COMMIT` with `COMMIT_UPLOADED`). It seals the extents written before it; cladefs: no
    /// window maps any of them, and nothing in a window changes.
    pub async fn commit(&self, root: Option<&[u8]>) -> Result<()> {
        self.commit_with(root, Commit::Uploaded).await
    }

    /// cladefs: commits as [`OpenCell::commit`] does, completing once local, synced to the
    /// host's disk, not waiting for the bucket (`COMMIT_LOCAL`, TASK-301).
    pub async fn commit_local(&self, root: Option<&[u8]>) -> Result<()> {
        self.commit_with(root, Commit::Local).await
    }

    /// cladefs: commits as [`OpenCell::commit`] does, but completing once ordered: a
    /// barrier, ordering what came before before what comes after (`COMMIT` without
    /// `COMMIT_UPLOADED`). [`OpenCell::flush`] makes the commits ordered so far local.
    pub async fn commit_ordered(&self, root: Option<&[u8]>) -> Result<()> {
        self.commit_with(root, Commit::Ordered).await
    }

    /// cladefs: closes the open segment at a commit, as a commit waiting for its upload does, and
    /// returns once every segment closed is framed: for tests of what framing costs (TASK-330.2).
    #[cfg(test)]
    pub(crate) async fn frame_open_segment(&self) -> Result<()> {
        self.store.close_segment().map_err(errno)?;
        self.commit_local(None).await?;
        self.store.wait_framed().await.map_err(errno)
    }

    /// Makes the commits ordered so far local.
    pub async fn flush(&self) -> Result<()> {
        self.store.flush().await.map_err(errno)
    }

    /// cladefs: `Ok` if the store takes a request changing `what` of it: the one gate every such
    /// request passes first, each refused for the same reasons, the first that holds: the store
    /// halted ([`crate::halt`]), which serves nothing more; its role ([`CellAccess`]); or its
    /// uploader failed for good ([`OpenCell::uploading`]), whose store's commits, and what they
    /// would hold, are its cell's no more (doc-4: `EROFS` on `WRITE`, `FREE` and `COMMIT` once
    /// another host took the head). Requests passing their own checks, each its own, let a halted
    /// store resize (TASK-611), a fenced one commit (TASK-550) and make versions (TASK-612).
    fn admit(&self, what: Changing) -> Result<()> {
        self.halt.check()?;
        let access = self.access();
        match what {
            Changing::Contents => access.may_write(),
            Changing::Capacity => access.may_resize(),
            Changing::Cell => access.may_manage(),
        }?;
        self.uploading()
    }

    /// cladefs: `Ok` unless the store's uploader stopped, failed: the store's commits are its
    /// cell's no more, as once a later placement took its cell (`EROFS`, TASK-364), so nothing
    /// changes it from then on ([`OpenCell::admit`]), as a kept image's commits fail
    /// (`storage::kept`).
    fn uploading(&self) -> Result<()> {
        match self.uploader.as_ref().and_then(|uploader| uploader.failed()) {
            Some(failure) => Err(upload_errno(failure)),
            None => Ok(()),
        }
    }

    /// Makes the commits ordered so far uploaded, if the store has an uploader; local, if not.
    async fn make_uploaded(&self) -> Result<()> {
        let epoch = self.make_local().await?;
        match self.uploader.as_ref() {
            Some(uploader) => {
                let uploaded = uploader.uploaded(epoch);
                upload_wait(epoch).run_async(uploaded).await.map_err(upload_errno)
            }
            None => Ok(()),
        }
    }

    /// cladefs: makes every commit ordered so far local, and returns the commit number of the
    /// latest: what [`OpenCell::uploaded`] waits for. It blocks on the store's I/O.
    pub async fn make_local(&self) -> Result<u64> {
        let epoch = self.store.last_commit().map_err(errno)?;
        self.flush().await?;
        Ok(epoch)
    }

    /// cladefs: returns once the commits up to `epoch` ([`OpenCell::make_local`]) are in
    /// object storage, as [`OpenCell::wait_uploaded`] does: a future, which holds no thread
    /// while it waits (TASK-43.6). `ENOTSUP` without an uploader.
    pub fn uploaded(
        &self,
        epoch: u64,
    ) -> impl std::future::Future<Output = Result<()>> + Send + use<> {
        let waiting = self.uploader.as_ref().map(|uploader| uploader.uploaded(epoch));
        async move {
            match waiting {
                Some(waiting) => waiting.await.map_err(upload_errno),
                None => Err(Errno::ENOTSUP),
            }
        }
    }

    /// cladefs: what the store's uploader has done, and has left to do, if it has one
    /// (TASK-50.5).
    pub fn upload_status(&self) -> Result<Option<local_cell::upload::UploadStatus>> {
        let uploading = self.uploader.as_ref().filter(|uploader| !uploader.is_at_version());
        uploading.map(|uploader| uploader.status().map_err(upload_errno)).transpose()
    }

    /// cladefs: waits until every commit ordered so far is uploaded, as before
    /// a clean stop (TASK-39.7). `ENOTSUP` without an uploader.
    pub async fn wait_uploaded(&self) -> Result<()> {
        if self.uploader.as_ref().is_none() {
            return Err(Errno::ENOTSUP);
        }
        self.make_uploaded().await
    }

    /// Whether commits ordered are not local yet: waiting for [`OpenCell::flush`], or taken
    /// by a flush in flight, which it waits for.
    pub fn flush_pending(&self) -> bool {
        self.store.flush_pending()
    }

    /// cladefs: counts a turn its daemon took at making its commits local, whether or not it
    /// flushed: a commit made before a turn began is local once the turn ends (TASK-110).
    pub fn flush_turned(&self) {
        self.flush_turns.fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// cladefs: the turns its daemon has taken at making its commits local ([`flush_turned`]).
    ///
    /// [`flush_turned`]: OpenCell::flush_turned
    pub fn flush_turns(&self) -> u64 {
        self.flush_turns.load(std::sync::atomic::Ordering::Acquire)
    }

    /// cladefs: in a span of its own, `store.commit` (`bucket_cell::trace`, TASK-258), whose
    /// children are its phases: `commit.seal` and `commit.write`, the store's; then, local or
    /// uploaded, `store.flush`, and `commit.upload_wait` as it waits for object storage.
    async fn commit_with(&self, root: Option<&[u8]>, commit: Commit) -> Result<()> {
        let span = trace::Span::start("store.commit");
        span.set_cell(self.cell());
        span.set(trace::LEVEL, commit.level());
        span.run_async(async {
            self.admit(Changing::Contents)?;
            let started = std::time::Instant::now();
            let committed = self.commit_and_wait(root, commit).await;
            stats::count_since(Counter::Commits, Counter::CommitNs, started);
            committed
        })
        .await
    }

    /// Commits, and waits as `commit` says: for the uploader's lag, the host's disk, or object
    /// storage; the commit's phases, each counted ([`stats::Stats::commits`]).
    async fn commit_and_wait(&self, root: Option<&[u8]>, commit: Commit) -> Result<()> {
        /// What a commit waits for: a level of [`Commit`]'s, or replicated (doc-21).
        enum Waiting {
            Commit(Commit),
            Replicated,
        }
        let committing = std::time::Instant::now();
        self.store.commit_ordered(root).await.map_err(errno)?;
        self.committed.notify(usize::MAX);
        stats::add_since(Counter::CommitStoreNs, committing);
        let waiting = std::time::Instant::now();
        // cladefs: in a replicated cell (doc-21, TASK-537.3.3), the guest's fsync waits for the
        // level its head asks for, and its sync family for its commits replicated, rather than
        // uploaded: on f+1 hosts' disks, or in the bucket.
        let replication = self.uploader.as_ref().and_then(|uploader| uploader.replication());
        let commit = match (commit, replication) {
            (Commit::Uploaded, Some(replicated)) if replicated.f >= 1 => Waiting::Replicated,
            (Commit::Ordered, Some(replicated)) => match replicated.fsync {
                bucket_cell::layout::Fsync::Ordered => Waiting::Commit(Commit::Ordered),
                bucket_cell::layout::Fsync::Local => Waiting::Commit(Commit::Local),
                bucket_cell::layout::Fsync::Replicated => Waiting::Replicated,
            },
            (commit, _) => Waiting::Commit(commit),
        };
        let waited = match commit {
            Waiting::Replicated => self.make_replicated().await,
            // cladefs: each level including the one before (doc-12): local, as ordered, within the
            // lag; uploaded, past it.
            Waiting::Commit(Commit::Uploaded) => self.make_uploaded().await,
            Waiting::Commit(Commit::Local) => match self.flush().await {
                Ok(()) => self.within_lag().await,
                Err(errno) => Err(errno),
            },
            Waiting::Commit(Commit::Ordered) => self.within_lag().await,
        };
        stats::add_since(Counter::CommitWaitNs, waiting);
        waited
    }

    /// cladefs: returns once the store's commits are within the lag its uploader allows (doc-12),
    /// which every commit level waits for: a lost host costs what is past the bucket, no more, and
    /// the host's disk holds no more (doc-14). Fails once the uploader failed for good, as no commit
    /// made since is its cell's (TASK-550, TASK-582).
    async fn within_lag(&self) -> Result<()> {
        let Some(uploader) = self.uploader.as_ref() else { return Ok(()) };
        let epoch = self.store.last_commit().map_err(errno)?;
        upload_wait(epoch).run_async(uploader.lag(epoch)).await.map_err(upload_errno)
    }

    /// cladefs: makes the store's commits so far replicated (doc-21): local, then on f members
    /// besides this host, or in the bucket, and within the lag the uploader allows.
    async fn make_replicated(&self) -> Result<()> {
        let epoch = self.make_local().await?;
        let Some(uploader) = self.uploader.as_ref() else { return Ok(()) };
        let (replicated, lag) = (uploader.replicated(epoch), uploader.lag(epoch));
        let waited = async {
            replicated.await?;
            lag.await
        };
        upload_wait(epoch).run_async(waited).await.map_err(upload_errno)
    }

    /// The root as of the last commit, empty if none set one (`ROOT`).
    pub fn root(&self) -> Result<Vec<u8>> {
        match &self.following {
            Some(following) => Ok(following.view.lock(&self.halt)?.root().to_vec()),
            None => self.store.root().map_err(errno),
        }
    }

    /// Creates the DAX window of `size` bytes that [`map`](Self::map) maps extents into, each
    /// change to whose mappings is made in `second_stage`, if given, rather than in this process's
    /// memory: the extent daemon's connection to the VM process (`crate::cladefsd`), which makes it
    /// in the VM's memory. The guest's faults on the window must be passed to
    /// [`DaxWindow::fault`], as the daemon passes those its VM process reports. A store has one
    /// window. cladefs: it refuses every store, as a follower's must; a writer's guest takes one
    /// that copies on write ([`create_copying_dax_window`](Self::create_copying_dax_window)).
    pub fn create_dax_window(
        &self,
        size: u64,
        second_stage: Option<Arc<dyn SecondStage>>,
    ) -> Result<DaxWindow, Error> {
        self.install_dax(|| Ok(Dax::new(size, second_stage)?))
    }

    /// cladefs: creates the DAX window as [`create_dax_window`](Self::create_dax_window) does, one
    /// that copies on write (backlog doc-13): a store to a page gives the page a private copy, of
    /// which it holds `private_max` pages at most, its second stage's memory, not the daemon's: none
    /// is reserved for them (TASK-501). A store whose extents can't change, a follower's, a version
    /// or a public base, takes none (as [`CellAccess::may_write`]).
    pub fn create_copying_dax_window(
        &self,
        size: u64,
        second_stage: Option<Arc<dyn SecondStage>>,
        private_max: u64,
    ) -> Result<DaxWindow, Error> {
        self.admit(Changing::Contents)?;
        let copying = crate::dax::CopyOnWrite::at_most(private_max);
        self.install_dax(|| Ok(Dax::copying(size, second_stage, Some(copying))?))
    }

    /// cladefs: the private pages the DAX window holds (backlog doc-13): none without a window, or
    /// with one that doesn't copy on write.
    pub fn private_pages(&self) -> Result<u64> {
        let dax = self.dax.lock(&self.halt)?;
        Ok(dax.as_ref().map_or(0, Dax::private_pages))
    }

    /// Creates the DAX window in the `size` bytes of host memory at `base`, for a hypervisor that
    /// follows host memory, such as KVM: the store takes the memory over, maps the window's pages
    /// there, and serves the faults on the rest with userfaultfd, on a thread of its own.
    /// `prefault`, if the hypervisor can map pages for the guest ahead of its accesses, is told of
    /// the pages mapped. All in this process, as tests have it: a VM process's window is the
    /// daemon's ([`CellClient`](crate::CellClient)'s of the same name).
    ///
    /// # Safety
    ///
    /// The memory must be a page-aligned mapping of `size` bytes that only the guest uses, until
    /// the store is dropped.
    #[cfg(all(test, target_os = "linux"))]
    unsafe fn create_dax_window_in_host_memory(
        &self,
        base: std::ptr::NonNull<u8>,
        size: u64,
        prefault: Option<Arc<dyn crate::Prefault>>,
    ) -> Result<DaxWindow, Error> {
        self.install_dax(|| {
            // SAFETY: The caller hands the memory over.
            let stage = unsafe {
                crate::dax::uffd::UffdSecondStage::in_process(
                    base.as_ptr() as usize,
                    size,
                    prefault,
                )
            }?;
            Dax::in_host_memory(stage)
        })
    }

    fn install_dax(
        &self,
        create: impl FnOnce() -> Result<(Dax, DaxWindow), Error>,
    ) -> Result<DaxWindow, Error> {
        let mut dax = self.dax.lock(&self.halt)?;
        if dax.is_some() {
            return Err(Errno::EEXIST.into());
        }
        let (new, window) = create()?;
        *dax = Some(new);
        Ok(window)
    }

    /// Maps `len` bytes at `offset` in `extent` into the DAX window at `window_offset`, read-only,
    /// replacing what was mapped there (`MAP`), the private pages there too. Offsets and length
    /// must be multiples of the page size. Without a window, fails with `ENOSYS`.
    ///
    /// The mapping is of a sealed extent's cached pages: cladefs: of an open one, it fails with
    /// `EPERM`, a guest's bug, counted (backlog doc-13). What of the range is past the extent
    /// reads as zeros, as the padding of its last page does.
    pub async fn map(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
        window_offset: u64,
    ) -> Result<()> {
        let start = std::time::Instant::now();
        let mapped = self.map_counted(extent, offset, len, window_offset).await;
        stats::count_since(Counter::Maps, Counter::MapNs, start);
        let result = mapped.as_ref().map(|_| ()).map_err(|errno| *errno);
        let change = Change::Map { extent, offset, result };
        self.changes.lock(&self.halt)?.record(window_offset, len, change);
        mapped
    }

    async fn map_counted(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
        window_offset: u64,
    ) -> Result<()> {
        let (cached, within) = self.mapped_of(extent, offset, len, window_offset).await?;
        let mut dax = self.dax.lock(&self.halt)?;
        let dax = dax.as_mut().ok_or(Errno::ENOSYS)?;
        if within > 0 {
            dax.map(window_offset, &cached, offset, within);
        }
        if len > within {
            dax.map_zeros(window_offset + within, len - within);
        }
        Ok(())
    }

    /// What a `MAP` of `len` bytes at `offset` in `extent` into the window at `window_offset`
    /// maps, checked: the extent's cached pages, and how many of the bytes they hold, those within
    /// the extent; zeros the rest.
    async fn mapped_of(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
        window_offset: u64,
    ) -> Result<(Arc<dyn Source>, u64)> {
        self.dax_range_checked(window_offset, len)?;
        // cladefs: a base's is of the cache's one copy of it.
        let base = self.base_holding(extent).await?;
        let extent_len = match (&base, &self.following) {
            (Some((_, len)), _) => *len,
            // A follower's view holds sealed extents alone.
            (None, Some(following)) => {
                let view = following.view.lock(&self.halt)?;
                self.store.view_extent(&view, extent).map_err(errno)?
            }
            // cladefs: an open extent maps nowhere: a guest's bug, counted, as a tripwire
            // (`open_maps_refused`).
            (None, None) => match self.store.extent(extent).map_err(errno)? {
                Some((_, true)) => return Err(refuse_open_map(extent)),
                Some((len, false)) => len,
                None => return Err(Errno::ENOENT),
            },
        };
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if !offset.is_multiple_of(page_size()) {
            return Err(Errno::EINVAL);
        }
        let cached: Arc<dyn Source> = match &base {
            Some((base, _)) => base.cached(extent, extent_len).await?,
            None => self.cached(extent, extent_len).await?,
        };
        let within = end.min(extent_len.next_multiple_of(page_size())).max(offset);
        Ok((cached, within - offset))
    }

    /// cladefs: maps each of `mappings`, in order, as [`OpenCell::map`] or
    /// [`OpenCell::map_zeros_at`] would, in one change of the window (`MAP_MANY`): the first
    /// that fails fails the call, those before it mapped. A snapshot's window's mappings
    /// ([`OpenCell::window_mappings`]) are mapped again so as its VM is restored, where a `MAP`
    /// each took a change of their own (TASK-92.14).
    pub async fn map_many(&self, mappings: &[WindowMapping]) -> Result<()> {
        let start = std::time::Instant::now();
        // What each maps, as far as the first that fails.
        let mut pieces = Vec::new();
        let mut failed = None;
        for (at, mapping) in mappings.iter().enumerate() {
            let WindowMapping { window_offset, len, contents } = *mapping;
            let mapped = match contents {
                MappingContents::Extent { extent, offset } => {
                    let mapped = self.mapped_of(extent, offset, len, window_offset).await;
                    mapped.map(|(cached, within)| {
                        if within > 0 {
                            let source = Some((cached, offset));
                            pieces.push(DaxMapping { offset: window_offset, len: within, source });
                        }
                        if len > within {
                            let (offset, len) = (window_offset + within, len - within);
                            pieces.push(DaxMapping { offset, len, source: None });
                        }
                    })
                }
                MappingContents::Zeros => self.dax_range_checked(window_offset, len).map(|()| {
                    pieces.push(DaxMapping { offset: window_offset, len, source: None });
                }),
            };
            if let Err(errno) = mapped {
                failed = Some((at, errno));
                break;
            }
        }
        self.dax.lock(&self.halt)?.as_mut().ok_or(Errno::ENOSYS)?.map_many(&pieces);
        // Each recorded as its own MAP would have it, the one that failed too, but zeros'.
        let tried = failed.map_or(mappings.len(), |(at, _)| at + 1);
        let mut changes = self.changes.lock(&self.halt)?;
        let mut maps = 0;
        for (at, mapping) in mappings[..tried].iter().enumerate() {
            let result = match failed {
                Some((failed, errno)) if failed == at => Err(errno),
                _ => Ok(()),
            };
            let change = match mapping.contents {
                MappingContents::Extent { extent, offset } => {
                    maps += 1;
                    Change::Map { extent, offset, result }
                }
                MappingContents::Zeros if result.is_ok() => Change::Zeros,
                MappingContents::Zeros => continue,
            };
            changes.record(mapping.window_offset, mapping.len, change);
        }
        stats::add(Counter::Maps, maps);
        stats::add_since(Counter::MapNs, start);
        failed.map_or(Ok(()), |(_, errno)| Err(errno))
    }

    /// Checks the DAX window's range for a mapping of `len` bytes at `window_offset`: `ENOSYS`
    /// without a window.
    fn dax_range_checked(&self, window_offset: u64, len: u64) -> Result<()> {
        let dax = self.dax.lock(&self.halt)?;
        dax.as_ref().ok_or(Errno::ENOSYS)?.check_range(window_offset, len)
    }

    /// Maps zeros, read-only, at `len` bytes of the DAX window at `window_offset`, as a mapping a
    /// snapshot recorded did ([`MappingContents::Zeros`]).
    pub fn map_zeros_at(&self, window_offset: u64, len: u64) -> Result<()> {
        let mut dax = self.dax.lock(&self.halt)?;
        let dax = dax.as_mut().ok_or(Errno::ENOSYS)?;
        dax.check_range(window_offset, len)?;
        self.changes.lock(&self.halt)?.record(window_offset, len, Change::Zeros);
        dax.map_zeros(window_offset, len);
        Ok(())
    }

    /// The DAX window's mappings, in order: what a snapshot of the VM records, to map them again
    /// where it is restored, at the same offsets, with [`OpenCell::map_many`]. Without a
    /// window, none. cladefs: `EBUSY` while the window holds private pages, which no mapping names
    /// the bytes of, as a snapshot's version is refused then (TASK-203).
    pub fn window_mappings(&self) -> Result<Vec<WindowMapping>> {
        let dax = self.dax.lock(&self.halt)?;
        match dax.as_ref() {
            Some(dax) if dax.private_pages() > 0 => Err(Errno::EBUSY),
            dax => Ok(dax.map_or_else(Vec::new, Dax::mappings)),
        }
    }

    /// cladefs: the DAX window's mappings, as [`OpenCell::window_mappings`] has them, its private
    /// pages or not: what a log says of a page.
    fn mapped(&self) -> Result<Vec<WindowMapping>> {
        let dax = self.dax.lock(&self.halt)?;
        Ok(dax.as_ref().map_or_else(Vec::new, Dax::mappings))
    }

    /// cladefs: notes the page at `window_offset` refused `what` (a guest's access), and returns
    /// what the window maps there, the extent's state here and in the store, and the page's
    /// latest changes, said for a log: why the access was refused (TASK-99). The next change of
    /// the window covering the page is logged too, with what it did, which the refusal met
    /// (TASK-44.12).
    pub fn refused(&self, window_offset: u64, what: &str) -> String {
        let mapped = self.describe_mapping(window_offset);
        let Ok(mut changes) = self.changes.lock(&self.halt) else {
            return format!("{mapped}; the store is halted");
        };
        let history = changes.of(window_offset);
        changes.refuse(window_offset, what, mapped.clone());
        format!("{mapped}; the page's latest MAPs and UNMAPs: {history:#?}")
    }

    /// cladefs: what the window maps at `window_offset`, and the extent's state here and in the
    /// store, said for a log.
    fn describe_mapping(&self, window_offset: u64) -> String {
        let Ok(mappings) = self.mapped() else {
            return "the store is halted".into();
        };
        let mapping = mappings.into_iter().find(|mapping| {
            (mapping.window_offset..mapping.window_offset + mapping.len).contains(&window_offset)
        });
        let Some(mapping) = mapping else {
            return "nothing mapped".into();
        };
        let MappingContents::Extent { extent, offset } = mapping.contents else {
            return format!("zeros, {:#x} bytes from {:#x}", mapping.len, mapping.window_offset);
        };
        let stored = self.store.extent(extent).map(|stored| {
            stored.map(|(len, open)| {
                format!("{len:#x} bytes, {}", if open { "open" } else { "sealed" })
            })
        });
        format!(
            "{extent:?} from {offset:#x}, {:#x} bytes from {:#x}; in the store: {stored:?}",
            mapping.len, mapping.window_offset,
        )
    }

    /// Forks the store as [`OpenCell::fork`] does, but only once nothing is open
    /// ([`OpenCell::is_quiescent`]), and no write comes between that and the fork, which would
    /// seal it under its writer: `EBUSY` otherwise. So a running VM's store forks,
    /// its guest's filesystem frozen (TASK-39.3), as a snapshot of the VM does (TASK-39.1).
    pub async fn fork_quiescent(
        &self,
        path: impl AsRef<Path>,
        tenant: u64,
    ) -> Result<CellId, Error> {
        let cell = self.new_child(tenant).await?;
        self.fork_at_rest(path.as_ref(), cell).await?;
        Ok(cell)
    }

    /// cladefs: forks the store as [`OpenCell::fork_quiescent`] does, to `cell`, whatever its
    /// ID, as [`OpenCell::fork_to`] forks it.
    #[cfg(test)]
    pub(crate) async fn fork_quiescent_to(
        &self,
        path: impl AsRef<Path>,
        cell: CellId,
    ) -> Result<(), Error> {
        self.fork_at_rest(path.as_ref(), cell).await
    }

    /// cladefs: makes a version of the store's cell (doc-18, doc-20): a name for its state as of its
    /// last commit, once nothing is open, as [`OpenCell::fork_quiescent`] checks. One local
    /// commit closing its segment, durable here when this returns; nothing goes to the bucket on
    /// the way: the cell's head names it once its segment is uploaded, behind. Of the versions,
    /// the newest `keep_last` are kept then, if given, the rest dropped. Returns its ID.
    /// `ENOTSUP` without an uploader, which names versions.
    pub async fn make_version(&self, keep_last: Option<u32>) -> Result<u64, Error> {
        // cladefs: its versions counted first, which awaits, then committed at rest, which holds
        // the store's opening until it is (TASK-685.3).
        self.within_versions(keep_last).await?;
        let id = self.at_rest(self.commit_version(keep_last, true)).await?;
        let named = self.versions().await.into_iter().map(|version| version.id).collect();
        host_cell::made_version(self.cell(), id, &self.path, &named);
        Ok(id)
    }

    /// cladefs: `Ok` if a version, keeping the newest `keep_last` if given, leaves the cell within
    /// the most versions its head names (`layout::MAX_VERSIONS`, doc-18): a window of them does,
    /// dropping the oldest; or fewer named, and made here, than the most. `EMLINK` otherwise, as
    /// a version made would be named, past it, by every acknowledgement after, whose head swaps
    /// write them all (TASK-570). Checked where versions are made, not named: a version made is
    /// told made, and named once uploaded, whatever.
    async fn within_versions(&self, keep_last: Option<u32>) -> Result<(), Error> {
        let most = bucket_cell::layout::MAX_VERSIONS;
        if keep_last.is_some_and(|keep| (keep as usize) <= most) {
            return Ok(());
        }
        let versions = self.versions().await.len();
        if versions >= most {
            let why = format!("the cell has {versions} versions, the most it may: drop some");
            return Err(Error::new(anyhow::Error::new(Errno::EMLINK).context(why)));
        }
        Ok(())
    }

    /// cladefs: commits a version as [`OpenCell::make_version`] does, of the store quiescent if
    /// `quiescent` (`LocalCell::commit_version`), and returns its ID.
    async fn commit_version(&self, keep_last: Option<u32>, quiescent: bool) -> Result<u64, Error> {
        self.uploader.as_ref().ok_or(Errno::ENOTSUP)?;
        let id = rand::random::<u64>().max(1);
        let version = bucket_cell::layout::NewVersion { id, keep_last };
        self.store.commit_version(version, quiescent).await.map_err(|err| {
            match err.is::<NotQuiescent>() {
                true => Error::new(anyhow::Error::new(Errno::EBUSY).context(err)),
                false => Error::new(err),
            }
        })?;
        Ok(id)
    }

    /// cladefs: the versions of the store's cell, oldest first: those its head names, uploaded,
    /// then those made here that it names not yet. None without an uploader.
    pub async fn versions(&self) -> Vec<VersionInfo> {
        let Some(uploader) = self.uploader.as_ref() else { return Vec::new() };
        let versions = uploader.versions().await;
        let uploaded = (versions.uploaded.into_iter()).map(|version| VersionInfo {
            id: version.id,
            uploaded: true,
            shared: version.shared,
        });
        let local = (versions.local.into_iter()).map(|(_, made)| VersionInfo {
            id: made.id,
            uploaded: false,
            shared: false,
        });
        uploaded.chain(local).collect()
    }

    /// cladefs: drops the versions `ids` names: the head names them no more, those not uploaded yet
    /// never. Forks of one keep what they inherited of it until they go.
    pub async fn drop_versions(&self, ids: &BTreeSet<u64>) -> Result<(), Error> {
        self.admit(Changing::Cell)?;
        let uploader = self.uploader.as_ref().ok_or(Errno::ENOTSUP)?;
        let dropped = uploader.drop_versions(ids).await.map_err(upload_errno);
        // cladefs: forgotten as made here, dropped or not: a drop that failed may have taken
        // effect, and one that didn't is the caller's to retry, which needs no store made here
        // (TASK-610).
        host_cell::dropped_versions(self.cell(), ids);
        Ok(dropped?)
    }

    /// cladefs: shares the version `id`, if `shared`, or no longer: forks of it into other tenants'
    /// cells are made while it is shared (`EACCES` otherwise), those made living on. Once every
    /// commit ordered so far is uploaded: a version made is named by the cell's head only once its
    /// segment is (`make_version`), by this store's writer or by the one a store let go of and
    /// opened again took the cell with, so that a version just made is shared, not refused as
    /// unknown (`ENOENT`).
    pub async fn share_version(&self, id: u64, shared: bool) -> Result<(), Error> {
        self.admit(Changing::Cell)?;
        let uploader = self.uploader.as_ref().ok_or(Errno::ENOTSUP)?;
        self.make_uploaded().await?;
        Ok(uploader.share_version(id, shared).await.map_err(upload_errno)?)
    }

    /// cladefs: the version of its cell it is at, if it is one ([`OpenCell::open_version_at`]).
    pub fn version(&self) -> Option<u64> {
        self.store.version()
    }

    /// cladefs: opens the version `id` of the store's cell at `path`, which must not exist, as the
    /// daemon opens a version for forks of it ([`OpenCell::create_at_version`]), once the version is
    /// uploaded: for tests of forks of versions.
    #[cfg(test)]
    pub(crate) fn open_version_at(&self, path: impl AsRef<Path>, id: u64) -> Result<(), Error> {
        let path = path.as_ref();
        let kind = self.store.kind().ok_or(Errno::EINVAL)?;
        let url = self.store.bucket().ok_or(Errno::EINVAL)?;
        Self::create_at_version(path, (url, kind), self.store.description(), id)
    }

    /// cladefs: makes a store at `path`, which must not exist, of the version `id` of the cell
    /// `description` names, of `kind`, kept in the bucket at `url`: what the daemon opens a version
    /// in for forks of it on a host, nothing writing it.
    pub fn create_at_version(
        path: &Path,
        (url, kind): (&str, Kind),
        description: local_cell::Description,
        id: u64,
    ) -> Result<(), Error> {
        host_fs::create_dir(path).map_err(|err| match err.kind() {
            std::io::ErrorKind::AlreadyExists => Error::new(Errno::EEXIST),
            _ => Error::new(anyhow::Error::new(err).context(format!("making {}", path.display()))),
        })?;
        let made = local_cell::create_host_at_version(path, description, url, kind, id)
            .and_then(|()| local_cell::made_host(path));
        if let Err(err) = made {
            crate::host_cell::discard(path);
            return Err(Error::new(err));
        }
        Ok(())
    }

    /// cladefs: makes a version, uploaded, and opens it at `path` as [`OpenCell::open_version_at`]
    /// does: for tests of forks of versions.
    #[cfg(test)]
    pub(crate) async fn uploaded_version_at(&self, path: impl AsRef<Path>) -> Result<u64, Error> {
        let id = self.make_version(None).await?;
        self.make_uploaded().await?;
        self.open_version_at(path, id)?;
        Ok(id)
    }

    /// cladefs: runs `then` once nothing is open, and none comes between: not while its window
    /// holds private pages, whose bytes no extent holds (`EBUSY`, TASK-203, backlog doc-13);
    /// `EBUSY` with an extent open.
    async fn at_rest<T>(&self, then: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        let private = self.private_pages()?;
        if private > 0 {
            let refused = format!(
                "the DAX window holds {private} private pages, whose bytes no extent holds: \
                 freeze the guest's filesystem, which rebinds them"
            );
            return Err(Error::new(anyhow::Error::new(Errno::EBUSY).context(refused)));
        }
        self.admit(Changing::Cell)?;
        let _opening = self.opening.write(&self.halt).await?;
        if !self.is_quiescent()? {
            let open = "an extent written since the last commit is open";
            return Err(Error::new(anyhow::Error::new(Errno::EBUSY).context(open)));
        }
        then.await
    }

    /// cladefs: forks the store as [`OpenCell::fork_quiescent`] says. Not while its window holds
    /// private pages (`EBUSY`), whose bytes are the VM's, held by no extent: a fork, or a VM
    /// snapshot's version, would name their sources, and a VM restored from it
    /// read their old bytes (TASK-203, backlog doc-13). The guest's freeze writes them back and
    /// rebinds them first; one it couldn't, as a writeback that found no memory leaves (TASK-207)
    /// or a rebind refused, it says it left. Checked before the fork's lock, which no write of the
    /// window's private pages takes: with its VM paused, as a snapshot has it, none comes after.
    async fn fork_at_rest(&self, path: &Path, cell: CellId) -> Result<(), Error> {
        self.fork_span(cell).run_async(self.fork_at_version(path, cell, true)).await
    }

    /// cladefs: whether the store holds what `other` does, as of their last commits: the same root,
    /// and the same extents, of the same lengths, its ancestors' among them. As a store a version
    /// was taken of does, written nothing since, and a fork of the version that wrote nothing: what
    /// a VM restored from a snapshot at the version may run on, its memory the version's (TASK-194).
    pub fn holds_what(&self, other: &OpenCell) -> Result<bool> {
        let held = |store: &LocalCell<HostDir>| -> anyhow::Result<_> {
            Ok((store.root()?, store.extents()?))
        };
        Ok(held(&self.store).map_err(errno)? == held(&other.store).map_err(errno)?)
    }

    /// Whether nothing is open: no extent written since the last commit, which a fork of the
    /// store as of that commit would not hold; as with its guest's filesystem frozen, a version a
    /// snapshot of its VM is taken at.
    pub fn is_quiescent(&self) -> Result<bool> {
        // cladefs: not, as far as anyone knows, once a panic went through it (`crate::halt`).
        self.halt.check()?;
        self.store.has_open().map(|open| !open).map_err(errno)
    }

    /// Unmaps `len` bytes of the DAX window at `window_offset` (`UNMAP`).
    pub fn unmap(&self, window_offset: u64, len: u64) -> Result<()> {
        let mut dax = self.dax.lock(&self.halt)?;
        let dax = dax.as_mut().ok_or(Errno::ENOSYS)?;
        dax.check_range(window_offset, len)?;
        self.changes.lock(&self.halt)?.record(window_offset, len, Change::Unmap);
        dax.unmap(window_offset, len);
        Ok(())
    }

    /// The sealed `extent`, of `len` bytes, in the cache: read from this store if no other store
    /// has it cached.
    async fn cached(&self, extent: ExtentId, len: u64) -> Result<Arc<CachedExtent>> {
        let fetcher = Some(self.fetcher());
        let recorder = self.profiling.as_ref().map(|(recorder, _)| recorder.clone());
        let cached_in = match self.public_reach(extent) {
            Some(reach) => CachedIn::Public(reach),
            None => CachedIn::Arena(self.arenas.of(extent)),
        };
        cache::cached(&self.store, cached_in, fetcher, recorder, extent, len).await
    }

    /// cladefs: that `extent` is reachable from a published generation, if it is: the store is
    /// public, a generation or a base its directory marks so, and holds it sealed (TASK-234).
    fn public_reach(&self, extent: ExtentId) -> Option<PublicReach> {
        if !self.is_public() {
            return None;
        }
        let held = self.store.extent(extent);
        matches!(held, Ok(Some((_, false)))).then_some(PublicReach(extent))
    }

    /// cladefs: makes the store's cell public, and those it inherited from (TASK-43): a base,
    /// which every tenant forks, whose extents' pages, and its ancestors', are of the default
    /// arena of physical memory, which every tenant's VM processes hold read-only.
    /// Stores opened since take it so; a tenant opens no public store itself, only forks of it.
    pub fn make_public(&self) -> Result<()> {
        self.admit(Changing::Cell)?;
        self.mark_public(&self.ancestry())
    }

    /// cladefs: makes the store's cell public, but not those it inherited from: a fork pinning a
    /// state of a store that goes on being written, which attaches to tenants' stores rather than
    /// being forked by them (backlog doc-10). Its pages are of the default arena through it
    /// ([`OpenCell::open_base`]).
    pub fn make_public_attach_only(&self) -> Result<()> {
        self.admit(Changing::Cell)?;
        self.mark_public(&[self.cell().id])
    }

    fn mark_public(&self, cells: &[u64]) -> Result<()> {
        cell_dir::mark_public(self.store.dir().path(), cells)
    }

    /// cladefs: the store's cell, and those it inherited from, nearest first.
    pub fn ancestry(&self) -> Vec<u64> {
        let inherited = self.forked_from.iter().map(|(cell, _)| *cell);
        std::iter::once(self.cell().id).chain(inherited).collect()
    }

    /// cladefs: attaches the store at `base`, a fork pinning what it holds, to this one, which
    /// `cells` of ([`OpenCell::ancestry`]), recorded in its directory: `READ` and `MAP` of
    /// its extents read through to it from now on, and in forks of this store, which a VM restored
    /// is. Attached already, it does nothing. The guest has no part in this (backlog doc-10).
    pub fn attach_base(&self, base: &Path, cells: &[u64]) -> Result<()> {
        self.admit(Changing::Cell)?;
        cell_dir::attach(&self.path, base, cells)?;
        self.bases.add(self.routed(cells), base);
        Ok(())
    }

    /// cladefs: of `cells`, those of a base's that its reads are routed by: not the store's own
    /// and those it inherited, whose extents it holds, or freed.
    fn routed(&self, cells: &[u64]) -> Vec<u64> {
        let own = self.ancestry();
        cells.iter().copied().filter(|cell| !own.contains(cell)).collect()
    }

    /// cladefs: routes reads to what is attached to the store, as recorded, each opened by `open`
    /// when first read: however many are attached, they cost it nothing until then.
    pub fn open_bases(&self, open: OpenBase) -> Result<()> {
        let _ = self.bases.open.set(open);
        for (cells, base) in cell_dir::attached(&self.path)? {
            self.bases.add(self.routed(&cells), &base);
        }
        Ok(())
    }

    /// cladefs: how many of what is attached to it are open.
    #[cfg(test)]
    pub(crate) fn bases_opened(&self) -> usize {
        self.bases.opened()
    }

    /// cladefs: the base attached that holds `extent`, with its length, if one does.
    async fn base_holding(&self, extent: ExtentId) -> Result<Option<(Arc<OpenCell>, u64)>> {
        let Some(bases) = self.bases.of(extent.cell) else { return Ok(None) };
        for base in bases {
            let store = base.store(self).await?;
            if let Some((len, false)) = store.store.extent(extent).map_err(errno)? {
                return Ok(Some((store, len)));
            }
        }
        Ok(None)
    }

    /// cladefs: whether the store's cell is public ([`OpenCell::make_public`]).
    pub fn is_public(&self) -> bool {
        self.arenas.public.contains(&self.cell().id)
    }

    /// cladefs: the arena of physical memory its own extents' pages are of
    /// ([`OpenCell::open_in`]): its tenant's, or the default.
    pub fn arena(&self) -> ArenaId {
        self.arenas.own()
    }
}

/// cladefs: that an extent is reachable from a published generation (TASK-234): a public store, a
/// generation or a base, holds it, so that every tenant attaching or forking it may read it. Only
/// a store makes one, having found so ([`OpenCell::public_reach`]): its field is this module's.
/// The extent cache moves a tenant's copy of an extent to the default arena, which every VM
/// process maps, only with one naming that extent (`cache::CachedIn::Public`).
pub(crate) struct PublicReach(ExtentId);

impl PublicReach {
    /// The extent reachable.
    pub(crate) fn extent(&self) -> ExtentId {
        self.0
    }
}

/// cladefs: the arenas of physical memory a store's extents' pages are of (TASK-43).
#[derive(Clone)]
struct Arenas {
    /// The store's, its tenant's, held for as long as the store and what reads into the extent
    /// cache for it, which clone these, allocate in it (TASK-79.36); or none, the default.
    own: Option<Arc<Arena>>,
    /// The public cells of its own and those it was forked from, as its directory marks them,
    /// whose extents' pages are of the default arena.
    public: Arc<std::collections::BTreeSet<u64>>,
}

impl Arenas {
    /// The store's arena.
    fn own(&self) -> ArenaId {
        self.own.as_ref().map_or(ArenaId::DEFAULT, |arena| arena.id())
    }

    /// The arena `extent`'s pages are of.
    fn of(&self, extent: ExtentId) -> ArenaId {
        if self.public.contains(&extent.cell) { ArenaId::DEFAULT } else { self.own() }
    }
}

/// cladefs: what opens a store attached to another, the first time it is read through to, awaited
/// (doc-24, step 5).
pub type OpenBase =
    Arc<dyn Fn(&Path) -> futures::future::BoxFuture<'static, Result<Arc<OpenCell>>> + Send + Sync>;

/// cladefs: a fork attached to a store (backlog doc-10).
struct Base {
    path: std::path::PathBuf,
    /// Opened once, by the first read through to it; those reading meanwhile wait for it, holding
    /// no thread.
    store: tokio::sync::OnceCell<Arc<OpenCell>>,
}

impl Base {
    fn new(path: &Path) -> Self {
        Self { path: path.to_owned(), store: tokio::sync::OnceCell::new() }
    }

    /// The fork, opened for `attached_to`, which is attached to it, once.
    async fn store(&self, attached_to: &OpenCell) -> Result<Arc<OpenCell>> {
        // cladefs: nothing a panic went through opens what it reads through to (`crate::halt`).
        attached_to.halt.check()?;
        let open = async || match attached_to.bases.open.get() {
            Some(open) => open(&self.path).await,
            None => {
                let arena = attached_to.arenas.own.clone();
                let opened = OpenCell::open_base(&self.path, arena).await.map_err(|err| {
                    log::warn!("cladefs: opening {}, attached: {err:#}", self.path.display());
                    err.errno().unwrap_or(Errno::EIO)
                })?;
                Ok(Arc::new(opened))
            }
        };
        self.store.get_or_try_init(open).await.cloned()
    }
}

/// cladefs: what is attached to a store: by the cells of the extents each holds, so that finding
/// what reads an extent takes the same time however many are attached. A cell has more than one
/// only for generations of one store, attached at once, the newest last.
#[derive(Default)]
struct Bases {
    open: OnceLock<OpenBase>,
    /// Shared by a store and its followers; unlike their locks, not halted
    /// ([`crate::halt`]): nothing that may panic is called holding it.
    by_cell: std::sync::RwLock<HashMap<u64, Vec<Arc<Base>>>>,
}

impl Bases {
    /// Routes `cells` to the store at `path`, if they aren't.
    fn add(&self, cells: Vec<u64>, path: &Path) {
        let mut by_cell = self.by_cell.write().unwrap();
        let attached = cells
            .iter()
            .filter_map(|cell| by_cell.get(cell))
            .flatten()
            .find(|base| base.path == path)
            .cloned();
        let base = attached.unwrap_or_else(|| Arc::new(Base::new(path)));
        for cell in cells {
            let routed = by_cell.entry(cell).or_default();
            if !routed.iter().any(|routed| Arc::ptr_eq(routed, &base)) {
                routed.push(base.clone());
            }
        }
    }

    /// Whether extents of `cell` are read through to what is attached.
    fn routes(&self, cell: u64) -> bool {
        self.by_cell.read().unwrap().contains_key(&cell)
    }

    /// What is attached that may hold extents of `cell`, newest first.
    fn of(&self, cell: u64) -> Option<Vec<Arc<Base>>> {
        let by_cell = self.by_cell.read().unwrap();
        by_cell.get(&cell).map(|bases| bases.iter().rev().cloned().collect())
    }

    /// How many of those attached are open.
    #[cfg(test)]
    fn opened(&self) -> usize {
        let mut opened: Vec<_> = (self.by_cell.read().unwrap().values().flatten())
            .filter(|base| base.store.initialized())
            .map(Arc::as_ptr)
            .collect();
        opened.sort();
        opened.dedup();
        opened.len()
    }
}

/// How many tasks read a boot profile's units into the cache at once.
const REPLAY_TASKS: usize = 4;

/// cladefs: reads `units`, a base's boot profile, into the extent cache from `store`, a fork of the
/// base, or from the bucket, through `fetcher`, for what it holds there, as a fork holds what it
/// inherited (TASK-165.7), in tasks of their own, on the fetcher's runtime (doc-24, step 5): those
/// of the extents the store holds, sealed, by a read of each unit's first byte, which pages in the
/// unit.
fn replay(
    store: Arc<LocalCell<HostDir>>,
    arenas: Arenas,
    fetcher: Arc<crate::bucket::HostFetcher>,
    units: Vec<profile::Unit>,
) {
    let units = Arc::new(units);
    for first in 0..REPLAY_TASKS {
        let (store, arenas, units) = (store.clone(), arenas.clone(), units.clone());
        let fetching = fetcher.clone();
        drop(fetcher.spawn(async move {
            for unit in units.iter().skip(first).step_by(REPLAY_TASKS) {
                let Ok(Some((len, false))) = store.extent(unit.extent) else { continue };
                let arena = arenas.of(unit.extent);
                let fetcher = Some(fetching.clone());
                let cached_in = CachedIn::Arena(arena);
                let extent = cache::cached(&store, cached_in, fetcher, None, unit.extent, len);
                let Ok(extent) = extent.await else { continue };
                match extent.read(unit.offset, &mut [0; 1]).await {
                    Ok(()) => stats::add(Counter::Prefetches, 1),
                    Err(errno) => log::debug!(
                        "cladefs: reading {:?} at {} ahead: {errno:?}",
                        unit.extent,
                        unit.offset
                    ),
                }
            }
        }));
    }
}

#[cfg(test)]
mod errno_tests {
    use super::{Errno, errno};

    /// TASK-539.12: the host's disk full, as making a store's file finds it, is `ENOSPC` to the
    /// guest; a store failed for it says so, and is `EIO`.
    #[test]
    fn a_full_disk_is_enospc_and_a_failed_store_eio() {
        let full = std::io::Error::from_raw_os_error(libc::ENOSPC);
        assert_eq!(errno(anyhow::Error::new(full).context("creating a raw file")), Errno::ENOSPC);
        let failed = anyhow::anyhow!("the extent store failed: framing a segment: no space");
        assert_eq!(errno(failed), Errno::EIO);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dax::{Access, SecondStage};
    use crate::pages::PhysLocation;
    use futures::executor::block_on;
    use std::sync::atomic::Ordering;

    /// An error of a read of `extent`, whose data is in the bucket.
    fn bucket_data(extent: ExtentId) -> local_cell::BucketData {
        let location = extent_frame::segment::Location { key: "k".to_owned(), offset: 0, len: 1 };
        local_cell::BucketData { extent, location }
    }

    /// Each of the store's errors has its errno, as doc-4 has them, a follower's among them, in
    /// context or not; any other is EIO (TASK-160.12).
    #[test]
    fn each_store_error_has_its_errno() {
        let extent = ExtentId { cell: 1, sequence: 2 };
        let cases: [(anyhow::Error, Errno); 13] = [
            (local_cell::CutOff.into(), Errno::ESTALE),
            (anyhow::Error::new(NoExtent(extent)).context(NotHeld(extent)), Errno::EIO),
            (NoExtent(extent).into(), Errno::ENOENT),
            (SealedExtent(extent).into(), Errno::EPERM),
            (SpentExtent(extent).into(), Errno::EPERM),
            (OtherCell(extent).into(), Errno::EPERM),
            (NoSpace.into(), Errno::ENOSPC),
            (TooSmall { capacity: 1, held: 2 }.into(), Errno::ENOSPC),
            (Unaligned { offset: 1, len: 2 }.into(), Errno::EINVAL),
            (Unsectored { offset: 1, len: 2 }.into(), Errno::EINVAL),
            (TooLong { offset: 1, len: 2 }.into(), Errno::EINVAL),
            (bucket_data(extent).into(), Errno::EIO),
            (anyhow::anyhow!("the disk"), Errno::EIO),
        ];
        for (error, expected) in cases {
            let said = format!("{error:#}");
            assert_eq!(errno(error.context("in context")), expected, "{said}");
        }
    }

    /// Each of the uploader's failures has its errno, as doc-4 and doc-7 have them: a cell this
    /// writer lost is EROFS, not EIO (TASK-160.12).
    #[test]
    fn each_uploader_failure_has_its_errno() {
        use bucket_cell::Error;
        use local_cell::upload::Failure;
        let cases = [
            (Failure::Bucket(Error::Fenced), Errno::EROFS),
            (Failure::Bucket(Error::Deleted), Errno::EROFS),
            (Failure::Bucket(Error::Stale), Errno::ESTALE),
            (Failure::Bucket(Error::Diverged), Errno::ESTALE),
            (Failure::Bucket(Error::NoCell), Errno::ENOENT),
            (Failure::VersionDropped, Errno::ENOENT),
            (Failure::Bucket(Error::IdTaken(bucket_cell::layout::test_cell(3))), Errno::EEXIST),
            (
                Failure::Bucket(Error::Format {
                    cell: bucket_cell::layout::test_cell(3),
                    format: 99,
                }),
                Errno::ENOTSUP,
            ),
            (Failure::Bucket(Error::Lost("a segment".into())), Errno::EIO),
            (Failure::Bucket(Error::Corrupt("a footer".into())), Errno::EIO),
            (Failure::Bucket(Error::Read("a file".into())), Errno::EIO),
            (Failure::msg("the uploader stopped"), Errno::EIO),
        ];
        for (failure, expected) in cases {
            let said = failure.to_string();
            assert_eq!(upload_errno(failure), expected, "{said}");
        }
    }

    const BLOCK: usize = OpenCell::BLOCK_SIZE as usize;

    /// TASK-44.12: a page refused waits for the next change of the window covering it, which
    /// meets it (and is logged), whatever the changes of other pages meanwhile.
    #[test]
    fn refused_pages_meet_the_next_change_covering_them() {
        let mut changes = WindowChanges::default();
        changes.refuse(0x3000, "a store", "an extent, read-only".into());
        changes.refuse(0x9000, "a store", "nothing mapped".into());
        changes.record(0x4000, 0x4000, Change::Unmap);
        let waiting = |changes: &WindowChanges| -> Vec<u64> {
            changes.refused.iter().map(|refused| refused.window_offset).collect()
        };
        assert_eq!(waiting(&changes), [0x3000, 0x9000]);
        changes.record(0x2000, 0x2000, Change::Zeros);
        assert_eq!(waiting(&changes), [0x9000]);
        changes.record(0x8000, 0x2000, Change::Unmap);
        assert_eq!(waiting(&changes), Vec::<u64>::new());
    }

    /// TASK-77: what a VM process asked of the window, recorded before it is refused, may reach
    /// past the end of a u64, with a page refused waiting.
    #[test]
    fn changes_past_the_end_of_a_u64_are_recorded() {
        let mut changes = WindowChanges::default();
        changes.refuse(0x3000, "a store", "an extent, read-only".into());
        changes.record(u64::MAX - 0xfff, 0x2000, Change::Unmap);
        assert_eq!(changes.refused.len(), 1);
        assert_eq!(changes.of(u64::MAX - 0xfff).len(), 1);
    }

    /// A new store of `capacity` bytes, in a directory of its own, kept in a memory bucket of the
    /// directory's, which the stores made in it share.
    async fn store(capacity: u64) -> (OpenCell, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (made(&dir.path().join("store"), capacity).await, dir)
    }

    /// A new store at `path`, of `capacity` bytes, kept in the memory bucket of the directory
    /// `path` is in ([`store`]).
    async fn made(path: &Path, capacity: u64) -> OpenCell {
        let named = path.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned();
        OpenCell::create_kept_in(path, capacity, &format!("memory:///{named}")).await.unwrap()
    }

    /// A new store, as [`store`] makes, whose uploader uploads nothing until a commit waits for it
    /// or the hour is up: what commits made local alone ([`committed_here`]) seal
    /// stays in its files, rather than leave for the bucket (TASK-165.9).
    async fn held(capacity: u64) -> (OpenCell, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().file_name().unwrap().to_string_lossy().into_owned();
        let keeping = local_cell::upload::Options {
            interval: std::time::Duration::from_secs(3600),
            ..Default::default()
        };
        let url = format!("memory:///{named}");
        let cell = host_cell::new_alone();
        let held =
            OpenCell::create_kept_with(dir.path().join("store"), capacity, cell, &url, keeping)
                .await;
        (held.unwrap(), dir)
    }

    /// Commits what `store` wrote, local alone: its uploader not waited for.
    async fn committed_here(store: &OpenCell) {
        store.commit_ordered(None).await.unwrap();
        store.flush().await.unwrap();
    }

    fn id(store: &OpenCell, sequence: u64) -> ExtentId {
        ExtentId { cell: store.cell().id, sequence }
    }

    /// `len` bytes of `byte` plus their offset modulo 251.
    fn pattern(byte: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| byte ^ (i % 251) as u8).collect()
    }

    async fn write(store: &OpenCell, extent: ExtentId, offset: u64, data: &[u8]) -> Result<()> {
        // In two buffers, as a request's descriptors may hold it.
        let (first, second) = data.split_at(data.len() / 2);
        store
            .write(extent, offset, &[VolatileSlice::from(first), VolatileSlice::from(second)])
            .await
    }

    async fn read(store: &OpenCell, extent: ExtentId, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut data = vec![0; len];
        let (first, second) = data.split_at_mut(BLOCK.min(len));
        store
            .read(
                extent,
                offset,
                &mut [VolatileSliceMut::from(first), VolatileSliceMut::from(second)],
            )
            .await?;
        Ok(data)
    }

    /// What [`holding_follows_the_extents`] does to a store: writes `blocks` blocks at block `block`
    /// of its cell's extent `sequence`, frees the extent `Free` picks of those it holds, its own or
    /// inherited, commits, opens it again, or forks it and goes on with the fork.
    #[derive(Clone, Debug)]
    enum CellChange {
        Write { sequence: u64, block: u64, blocks: usize },
        Free(proptest::sample::Index),
        Commit,
        Reopen,
        Fork,
    }

    fn store_change() -> impl proptest::strategy::Strategy<Value = CellChange> {
        use proptest::prelude::*;
        prop_oneof![
            4 => (1..12u64, 0..3u64, 1..3usize)
                .prop_map(|(sequence, block, blocks)| CellChange::Write { sequence, block, blocks }),
            3 => any::<proptest::sample::Index>().prop_map(CellChange::Free),
            2 => Just(CellChange::Commit),
            1 => Just(CellChange::Reopen),
            1 => Just(CellChange::Fork),
        ]
    }

    /// Panics unless what `store` says it holds ([`OpenCell::holding`]), the counts kept as its
    /// extents come and go, is what its extents, all walked, hold: how many, own and inherited, and
    /// their bytes (TASK-454).
    fn holding_is_recounted(store: &OpenCell, after: &[CellChange]) {
        let cell = store.cell().id;
        let held = store.store.extents().unwrap();
        let own: Vec<_> = held.iter().filter(|(extent, _)| extent.cell == cell).collect();
        let bytes = |held: &mut dyn Iterator<Item = &(ExtentId, u64)>| -> u64 {
            held.map(|(_, len)| len).sum()
        };
        let (own_bytes, all_bytes) = (bytes(&mut own.iter().copied()), bytes(&mut held.iter()));
        let recounted = CellHolding {
            bytes: Whose { own: own_bytes, inherited: all_bytes - own_bytes },
            extents: Whose { own: own.len() as u64, inherited: (held.len() - own.len()) as u64 },
        };
        assert_eq!(store.holding().unwrap(), recounted, "after {after:?}");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(32))]

        /// TASK-454: a store's counts of its extents, own and inherited, and of their bytes, which
        /// it keeps on as they are written, freed, committed, opened again and forked, are what
        /// its extents hold, recounted after each change.
        #[test]
        fn holding_follows_the_extents(changes in proptest::collection::vec(store_change(), 1..24)) {
            // Each case on a runtime of its own: proptest runs them on the test's thread.
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                let dir = tempfile::tempdir().unwrap();
                let mut path = dir.path().join("store");
                let mut store = made(&path, 1 << 30).await;
                for (done, change) in changes.iter().enumerate() {
                    match change {
                        CellChange::Write { sequence, block, blocks } => {
                            let data = pattern(*sequence as u8, blocks * BLOCK);
                            let at = block * BLOCK as u64;
                            // Refused if sealed or spent: the counts stand.
                            match write(&store, id(&store, *sequence), at, &data).await {
                                Ok(()) | Err(Errno::EPERM) => {}
                                Err(err) => panic!("writing: {err:?}"),
                            }
                        }
                        CellChange::Free(pick) => {
                            let held = store.store.extents().unwrap();
                            if !held.is_empty() {
                                let (extent, _) = held[pick.index(held.len())];
                                store.free(extent).unwrap();
                            }
                        }
                        CellChange::Commit => store.commit(None).await.unwrap(),
                        CellChange::Reopen => {
                            drop(store);
                            store = OpenCell::open(&path).await.unwrap();
                        }
                        CellChange::Fork => {
                            store.commit(None).await.unwrap();
                            let fork = dir.path().join(format!("fork-{done}"));
                            store.fork_to(&fork, crate::host_cell::fork_child(store.cell())).await.unwrap();
                            drop(store);
                            (path, store) = (fork.clone(), OpenCell::open(&fork).await.unwrap());
                        }
                    }
                    holding_is_recounted(&store, &changes[..=done]);
                }
            });
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn serves_extents() {
        let (store, dir) = store(1 << 20).await;
        let (a, b) = (id(&store, 1), id(&store, 2));
        write(&store, a, 0, &pattern(1, 2 * BLOCK)).await.unwrap();
        write(&store, b, BLOCK as u64, &pattern(2, BLOCK)).await.unwrap();
        assert_eq!(read(&store, a, 0, 2 * BLOCK).await, Ok(pattern(1, 2 * BLOCK)));
        assert_eq!(
            read(&store, b, 0, 2 * BLOCK).await,
            Ok([vec![0; BLOCK], pattern(2, BLOCK)].concat())
        );
        store.commit(Some(b"root")).await.unwrap();
        write(&store, id(&store, 3), 0, &pattern(3, BLOCK)).await.unwrap();
        store.free(b).unwrap();

        // As doc-4 has them: an extent not held, a sealed one, another cell's, unaligned I/O, and
        // too much.
        assert_eq!(read(&store, b, 0, BLOCK).await, Err(Errno::ENOENT));
        assert_eq!(write(&store, a, 2 * BLOCK as u64, &[0; BLOCK]).await, Err(Errno::EPERM));
        let other = ExtentId { cell: store.cell().id + 1, sequence: 1 };
        assert_eq!(write(&store, other, 0, &[0; BLOCK]).await, Err(Errno::EPERM));
        assert_eq!(write(&store, id(&store, 4), 1, &[0; BLOCK]).await, Err(Errno::EINVAL));
        assert_eq!(write(&store, id(&store, 4), 0, &vec![0; 1 << 20]).await, Err(Errno::ENOSPC));
        let info = store.info().unwrap();
        assert_eq!((info.used, info.next_sequence), (3 * BLOCK as u64, 4));

        // What wasn't committed is gone once the store is opened again.
        let cell = store.cell();
        drop(store);
        let store = OpenCell::open(dir.path().join("store")).await.unwrap();
        assert_eq!(store.cell(), cell);
        assert_eq!(store.root(), Ok(b"root".to_vec()));
        assert_eq!(read(&store, b, BLOCK as u64, BLOCK).await, Ok(pattern(2, BLOCK)));
        assert_eq!(read(&store, id(&store, 3), 0, BLOCK).await, Err(Errno::ENOENT));
    }

    /// doc-24, step 9: a store its holders let go of ([`release`]) closes as its last reference
    /// goes, and not before: let go of by one while another holds it, it writes and commits still;
    /// let go of by the last, it closes, what it committed kept, and opens again holding it.
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_close_once_their_last_reference_is_released() {
        let (store, dir) = store(1 << 20).await;
        let (a, b) = (id(&store, 1), id(&store, 2));
        write(&store, a, 0, &pattern(1, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        let held = Arc::new(store);
        release(held.clone()).await;
        write(&held, b, 0, &pattern(2, BLOCK)).await.unwrap();
        held.commit_ordered(None).await.unwrap();
        let cell = held.cell();
        release(held).await;
        let store = OpenCell::open(dir.path().join("store")).await.unwrap();
        assert_eq!(store.cell(), cell);
        assert_eq!(read(&store, a, 0, BLOCK).await, Ok(pattern(1, BLOCK)));
        assert_eq!(read(&store, b, 0, BLOCK).await, Ok(pattern(2, BLOCK)));
        store.close().await;
    }

    /// TASK-411.3: a store of a tenant whose KEK the host's KMS holds (`CLADEFS_KMS`, set here as
    /// the test's process's): its cell made encrypted, its cell given a DEK, and no byte of what
    /// it wrote in its bucket; read back, by a follower, and opened again. TASK-411.4: its fork into
    /// a new cell keeps the DEK it inherits rewrapped under its own cell's KEK, and reads what it
    /// inherits. TASK-411.28: a tenant with no KEK there gets no store, refused as destroyed
    /// (`EACCES`), and the store, the common tenant's, forks into no cell of its (`EACCES`), nothing
    /// made.
    #[tokio::test(flavor = "multi_thread")]
    async fn encrypted_stores_serve_follow_reopen_and_fork_into_encrypted_cells_alone() {
        let keys = tempfile::tempdir().unwrap();
        let kms = extent_keys::kms::LocalKms::open(keys.path().join("kms")).unwrap();
        kms.create(5).unwrap();
        kms.create(host_cell::DEFAULT_TENANT).unwrap();
        crate::bucket::set_kms(Arc::new(kms)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let store = OpenCell::create_in(&path, 1 << 20, 5).await.unwrap();
        assert!(!store.store.keys().is_empty());
        let store = Arc::new(store);
        let a = id(&store, 1);
        let data = crate::test_data::noise(1, 2 * BLOCK);
        write(&store, a, 0, &data).await.unwrap();
        store.commit(Some(b"root")).await.unwrap();
        let follower = store.follow();
        assert_eq!(read(&follower, a, 0, 2 * BLOCK).await, Ok(data.clone()));
        assert_eq!(read(&store, a, 0, 2 * BLOCK).await, Ok(data.clone()));
        // Nothing of it in the bucket: every file under the store's own, as on the disk, what the
        // store's page cache holds written there first (TASK-433).
        local_cell::pages::shared().write_back_all().unwrap();
        let bucket = dir.path().join("store.bucket");
        let mut files = vec![bucket.clone()];
        let mut objects = 0;
        while let Some(at) = files.pop() {
            match host_fs::is_dir(&at) {
                true => files.extend(host_fs::read_dir(&at).unwrap().map(|e| e.unwrap().path())),
                false => {
                    let bytes = host_fs::read(&at).unwrap();
                    let held = bytes.windows(64).any(|window| window == &data[..64]);
                    assert!(!held, "{} holds the store's data", at.display());
                    objects += 1;
                }
            }
        }
        assert!(objects > 3, "{objects} objects");
        let child = host_cell::fork_child(store.cell());
        store.fork_to(dir.path().join("fork"), child).await.unwrap();
        let fork = OpenCell::open(dir.path().join("fork")).await.unwrap();
        assert_eq!(read(&fork, a, 0, 2 * BLOCK).await, Ok(data.clone()));
        let record = bucket_cell::head::get(&**fork.bucket(), child).await;
        let record = record.unwrap().unwrap();
        let inherited: Vec<_> = record.inherited.iter().map(|had| had.cell).collect();
        assert_eq!((record.deks.len(), inherited), (1, vec![store.cell()]));
        drop(fork);
        drop(follower);
        let cell = store.cell();
        drop(Arc::into_inner(store).unwrap());
        let store = OpenCell::open(&path).await.unwrap();
        assert_eq!((store.cell(), store.root()), (cell, Ok(b"root".to_vec())));
        assert_eq!(read(&store, a, 0, 2 * BLOCK).await, Ok(data));

        // A store of a tenant with no KEK there: none, its cell refused, as destroyed.
        let none = OpenCell::create_in(dir.path().join("none"), 1 << 20, 6).await;
        let refused = none.err().expect("a store of a tenant with no KEK");
        assert_eq!(refused.errno(), Some(Errno::EACCES), "{refused:#}");
        assert!(format!("{refused:#}").contains("tenant 6: no KEK in the KMS"), "{refused:#}");

        // The default tenant's, encrypted, into a cell of a tenant with no KEK.
        let common = dir.path().join("common");
        let common = OpenCell::create_in(&common, 1 << 20, host_cell::DEFAULT_TENANT).await;
        let common = common.unwrap();
        assert!(!common.store.keys().is_empty());
        write(&common, id(&common, 1), 0, &pattern(2, BLOCK)).await.unwrap();
        common.commit(None).await.unwrap();
        let into = crate::host_cell::named(6);
        let refused = common.fork_to(dir.path().join("into-none"), into).await.unwrap_err();
        assert_eq!(refused.errno(), Some(Errno::EACCES), "{refused:#}");
        assert!(!host_fs::exists(&dir.path().join("into-none")));
        let found = bucket_cell::head::get(&**common.bucket(), into).await;
        assert_eq!(found.unwrap(), None);
    }

    /// TASK-411.18: a store of a tenant whose KEK is a key in Google Cloud KMS, `CLADEFS_KMS` naming
    /// it as a host is configured (`gcp-kms://...`): its cell made encrypted, its KEK wrapped by the
    /// tenant's key, which unwraps it to the KEK its cell's DEK is wrapped under; written, read
    /// back, opened again and forked into an encrypted cell. Skipped, saying why, without an access
    /// token (`extent_keys::testing::gcp`).
    #[tokio::test(flavor = "multi_thread")]
    async fn encrypted_stores_open_through_google_cloud_kms() {
        let test = "encrypted_stores_open_through_google_cloud_kms";
        let Some(admin) = extent_keys::testing::gcp::Admin::from_env(test) else { return };
        let url = admin.url("fxdax-test-{tenant}", None);
        admin.key("fxdax-test-5").await;
        // SAFETY: nextest runs each test in a process of its own, this one's other threads not
        // reading the environment yet.
        unsafe { std::env::set_var("CLADEFS_KMS", &url) };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store");
        let store = OpenCell::create_in(&path, 1 << 20, 5).await.unwrap();
        let dek = *store.store.keys().encrypting().0.expose();
        let a = id(&store, 1);
        let data = crate::test_data::noise(1, 2 * BLOCK);
        write(&store, a, 0, &data).await.unwrap();
        store.commit(Some(b"root")).await.unwrap();
        assert_eq!(read(&store, a, 0, 2 * BLOCK).await, Ok(data.clone()));

        let cell = store.cell();
        let bucket = store.bucket().clone();
        let object = bucket_cell::head::get(&*bucket, cell).await;
        let object = object.unwrap().unwrap();
        let [kek] = &object.keks[..] else { panic!("{:?}", object.keks) };
        assert!(kek.wrapped.0.starts_with(b"fxkr\x01\x07gcp-kms"), "{:?}", kek.wrapped);
        let kms = extent_keys::kms::from_url(&url).unwrap();
        let unwrapped = kms.unwrap_dyn(5, &kek.wrapped).await;
        let unwrapped = unwrapped.unwrap();
        let record = bucket_cell::head::get(&*bucket, cell).await;
        let record = record.unwrap().unwrap();
        let wrapped_dek = bucket_cell::keys::unwrap_dek(&unwrapped, cell, &record.deks[0]);
        assert_eq!(wrapped_dek.unwrap().expose(), &dek);
        let other = kms.unwrap_dyn(6, &kek.wrapped).await;
        assert_eq!(
            other.map(drop).unwrap_err().to_string(),
            "tenant 6: not authentic: a wrapped key, wrapped under another key"
        );

        drop(store);
        let store = OpenCell::open(&path).await.unwrap();
        assert_eq!((store.cell(), store.root()), (cell, Ok(b"root".to_vec())));
        assert_eq!(read(&store, a, 0, 2 * BLOCK).await, Ok(data.clone()));
        let child = host_cell::fork_child(store.cell());
        store.fork_to(dir.path().join("fork"), child).await.unwrap();
        let fork = OpenCell::open(dir.path().join("fork")).await.unwrap();
        assert!(!fork.store.keys().is_empty());
        assert_eq!(read(&fork, a, 0, 2 * BLOCK).await, Ok(data));
    }

    /// Reads of sealed extents come from the extent cache (TASK-32): a fork reading what its base
    /// read before reads nothing from the store. Past an extent's end, zeros, as the store reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn reads_of_sealed_extents_come_from_the_cache() {
        let (store, dir) = store(1 << 20).await;
        let extent = id(&store, 1);
        write(&store, extent, 0, &pattern(1, 2 * BLOCK)).await.unwrap();
        // Open: from the store.
        let before = stats::stats();
        assert_eq!(read(&store, extent, 0, 2 * BLOCK).await, Ok(pattern(1, 2 * BLOCK)));
        assert_eq!(stats::stats().since(&before).read_stored_bytes, 2 * BLOCK as u64);
        store.commit(None).await.unwrap();

        let before = stats::stats();
        let tail = [pattern(1, 2 * BLOCK)[BLOCK..].to_vec(), vec![0; BLOCK]].concat();
        assert_eq!(read(&store, extent, BLOCK as u64, 2 * BLOCK).await, Ok(tail));
        let first = stats::stats().since(&before);
        assert_eq!((first.read_stored_bytes, first.page_ins), (0, 1));

        let path = dir.path().join("fork");
        store.fork_to(&path, crate::host_cell::fork_child(store.cell())).await.unwrap();
        let fork = OpenCell::open(&path).await.unwrap();
        let before = stats::stats();
        assert_eq!(read(&fork, extent, 0, 2 * BLOCK).await, Ok(pattern(1, 2 * BLOCK)));
        let again = stats::stats().since(&before);
        assert_eq!((again.read_stored_bytes, again.page_ins, again.page_in_bytes), (0, 0, 0));
    }

    /// A follower reads its writer's store as of its view's commit while the writer goes on: what
    /// the writer freed since, still; what it wrote since, not (ENOENT, as a journal's end); and,
    /// advanced, what the commits between held until it lets go of them, and then, loudly (EIO),
    /// not.
    #[tokio::test(flavor = "multi_thread")]
    async fn followers_read_their_views_commits() {
        let (store, _dir) = store(1 << 20).await;
        let store = Arc::new(store);
        let (a, b) = (id(&store, 1), id(&store, 2));
        write(&store, a, 0, &pattern(1, BLOCK)).await.unwrap();
        store.commit(Some(b"one")).await.unwrap();
        let follower = store.follow();
        assert_eq!(follower.access().role, CellRole::Follower);
        store.free(a).unwrap();
        write(&store, b, 0, &pattern(2, BLOCK)).await.unwrap();
        store.commit(Some(b"two")).await.unwrap();
        assert_eq!(read(&follower, a, 0, BLOCK).await, Ok(pattern(1, BLOCK)));
        assert_eq!(read(&follower, b, 0, BLOCK).await, Err(Errno::ENOENT));
        assert_eq!(follower.root(), Ok(b"one".to_vec()));

        let commit = follower.advance().unwrap();
        assert_eq!(follower.view_commit(), Ok(commit));
        assert_eq!(follower.root(), Ok(b"two".to_vec()));
        assert_eq!(read(&follower, b, 0, BLOCK).await, Ok(pattern(2, BLOCK)));
        assert_eq!(read(&follower, a, 0, BLOCK).await, Ok(pattern(1, BLOCK)));
        follower.let_go(commit).unwrap();
        assert_eq!(read(&follower, a, 0, BLOCK).await, Err(Errno::EIO));
        // Pinned, an extent stays, whatever commit the follower is as of.
        follower.pin(b).unwrap();
        store.free(b).unwrap();
        store.commit(None).await.unwrap();
        let commit = follower.advance().unwrap();
        follower.let_go(commit).unwrap();
        assert_eq!(read(&follower, b, 0, BLOCK).await, Ok(pattern(2, BLOCK)));
        follower.unpin(b).unwrap();
        assert_eq!(read(&follower, b, 0, BLOCK).await, Err(Errno::EIO));

        // Dropped, it leaves its writer's store open.
        drop(follower);
        write(&store, id(&store, 3), 0, &pattern(3, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
    }

    /// TASK-165.8: a follower reads what its view's commit holds that the writer freed after it in
    /// the bucket, the segment holding it having left the store once acknowledged there; and once
    /// the bucket's collector has reclaimed it, which no later commit holds, the follower is cut
    /// off (`ESTALE`), as one too far behind its writer is, rather than fail its guest (`EIO`).
    #[tokio::test(flavor = "multi_thread")]
    async fn followers_are_cut_off_once_the_collector_reclaims_what_they_read() {
        let (store, dir) = store(1 << 20).await;
        let store = Arc::new(store);
        let a = id(&store, 1);
        write(&store, a, 0, &pattern(1, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        let follower = store.follow();
        store.free(a).unwrap();
        store.commit(None).await.unwrap();
        let segments =
            dir.path().join("store/segments").join(base32_id::Padded(store.cell().id).to_string());
        assert_eq!(host_fs::read_dir(&segments).unwrap().count(), 0, "segments left in the store");
        assert_eq!(read(&follower, a, 0, BLOCK).await, Ok(pattern(1, BLOCK)));
        // Commits, each of a segment, until the uploader checkpoints the cell, which then holds
        // the history the collector goes by: an extent written, and freed by the next; the
        // checkpoint in the bucket before the collector lists it (TASK-339).
        crate::test_checkpoints::after(&store, store.bucket(), async || {
            for sequence in 2..2 + local_cell::upload::CHECKPOINT_EVERY {
                write(&store, id(&store, sequence), 0, &pattern(sequence as u8, BLOCK))
                    .await
                    .unwrap();
                store.commit(None).await.unwrap();
                store.free(id(&store, sequence)).unwrap();
            }
            store.commit(None).await.unwrap();
        })
        .await;
        let options = bucket_cell::collector::Options { compact_below: 0.5, grace: 0 };
        let collected = bucket_cell::collector::collect(&**store.bucket(), 1, &options);
        let report = futures::executor::block_on(collected).unwrap();
        // All the checkpoint covers but the writer's fence, which the collector keeps (TASK-371).
        assert_eq!(
            report,
            bucket_cell::collector::Report { deleted: 64, compacted: 0, retired: 0, ..report }
        );
        assert_eq!(read(&follower, a, 0, BLOCK).await, Err(Errno::ESTALE));
    }

    /// A follower changes nothing: what would, it refuses with EROFS.
    #[tokio::test(flavor = "multi_thread")]
    async fn followers_change_nothing() {
        let (store, dir) = store(1 << 20).await;
        let store = Arc::new(store);
        let a = id(&store, 1);
        write(&store, a, 0, &pattern(1, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        let follower = store.follow();
        assert_eq!(write(&follower, id(&store, 2), 0, &pattern(2, BLOCK)).await, Err(Errno::EROFS));
        assert_eq!(follower.free(a), Err(Errno::EROFS));
        assert_eq!(follower.commit(None).await, Err(Errno::EROFS));
        assert_eq!(follower.commit_ordered(None).await, Err(Errno::EROFS));
        assert_eq!(follower.set_capacity(2 << 20).await, Err(Errno::EROFS));
        let forked = follower.fork_to(dir.path().join("fork"), host_cell::new_alone()).await;
        assert_eq!(forked.map_err(|err| err.errno()), Err(Some(Errno::EROFS)));
        // cladefs: a window that copies on write, nor so.
        let copying = follower.create_copying_dax_window(4 * page_size(), None, 1);
        assert_eq!(copying.map_err(|err| err.errno()).map(drop), Err(Some(Errno::EROFS)));
        assert_eq!(store.follow().advance().map(|_| ()), Ok(()));
        assert_eq!(store.advance(), Err(Errno::EINVAL));
    }

    /// A follower maps what its view's commit holds, sharing the cached pages the writer maps;
    /// what the writer wrote since, it doesn't (ENOENT).
    #[tokio::test(flavor = "multi_thread")]
    async fn followers_map_their_views_commits() {
        let (store, _dir) = store(1 << 20).await;
        let store = Arc::new(store);
        let ps = page_size();
        let a = id(&store, 1);
        write(&store, a, 0, &pattern(4, ps as usize)).await.unwrap();
        store.commit(None).await.unwrap();
        let follower = store.follow();
        let b = id(&store, 2);
        write(&store, b, 0, &pattern(5, ps as usize)).await.unwrap();
        store.commit(None).await.unwrap();
        let window = follower.create_dax_window(4 * ps, None).unwrap();
        follower.map(a, 0, ps, 0).await.unwrap();
        assert_eq!(follower.map(b, 0, ps, ps).await, Err(Errno::ENOENT));
        block_on(window.fault(0, Access::Read)).unwrap();
        // SAFETY: The test faulted the page in, and nothing else accesses it.
        let bytes = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
        assert!(bytes == pattern(4, ps as usize), "the follower's window maps other data");
    }

    /// A new writer takes a store over from its followers, or from a writer that went, once
    /// nothing is open in it, which they go on following; its window goes with the writer that
    /// went. Cut off, the followers fail with ESTALE.
    #[tokio::test(flavor = "multi_thread")]
    async fn writers_take_over_stores_followed() {
        let (store, _dir) = store(1 << 20).await;
        let store = Arc::new(store);
        let follower = store.follow();
        assert_eq!(follower.take_over(), Err(Errno::EROFS));
        store.take_over().unwrap();
        let _window = store.create_dax_window(4 * page_size(), None).unwrap();
        let a = id(&store, 1);
        write(&store, a, 0, &pattern(1, BLOCK)).await.unwrap();
        assert_eq!(store.take_over(), Err(Errno::EBUSY));
        store.commit(None).await.unwrap();
        store.take_over().unwrap();
        let _window = store.create_dax_window(4 * page_size(), None).unwrap();
        follower.advance().unwrap();
        assert_eq!(read(&follower, a, 0, BLOCK).await, Ok(pattern(1, BLOCK)));

        store.cut_followers();
        assert_eq!(read(&follower, a, 0, BLOCK).await, Err(Errno::ESTALE));
        assert_eq!(follower.advance(), Err(Errno::ESTALE));
        assert_eq!(follower.pin(a), Err(Errno::ESTALE));
        let fresh = store.follow();
        assert_eq!(read(&fresh, a, 0, BLOCK).await, Ok(pattern(1, BLOCK)));
    }

    /// A follower learns of its writer's commits after its view's.
    #[tokio::test(flavor = "multi_thread")]
    async fn followers_hear_of_commits() {
        let (store, _dir) = store(1 << 20).await;
        let store = Arc::new(store);
        store.commit(None).await.unwrap();
        let follower = store.follow();
        let commit = follower.view_commit().unwrap();
        assert_eq!(follower.newer(commit, std::time::Duration::from_millis(10)), None);
        write(&store, id(&store, 1), 0, &pattern(1, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        let newer = follower.newer(commit, std::time::Duration::from_secs(10));
        assert_eq!(newer, Some(follower.advance().unwrap()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn maps_sealed_extents() {
        let (store, _dir) = store(1 << 20).await;
        let ps = page_size();
        let extent = id(&store, 1);
        // Not a whole number of pages where pages are 16 KiB.
        let len = ps as usize + BLOCK;
        write(&store, extent, 0, &pattern(5, len)).await.unwrap();
        assert_eq!(store.map(extent, 0, ps, 0).await, Err(Errno::ENOSYS));
        let window = store.create_dax_window(6 * ps, None).unwrap();
        store.commit(None).await.unwrap();
        store.map(extent, 0, 2 * ps, ps).await.unwrap();
        // Past the extent, zeros.
        store.map(extent, 0, 3 * ps, 3 * ps).await.unwrap();
        assert_eq!(store.map(id(&store, 9), 0, ps, 0).await, Err(Errno::ENOENT));

        block_on(async {
            window.fault(ps, Access::Read).await.unwrap();
            window.fault(2 * ps, Access::Read).await.unwrap();
        });
        // SAFETY: The test faulted the pages in, and nothing else accesses them.
        let bytes = unsafe {
            std::slice::from_raw_parts(window.host_base().add(ps as usize), 2 * ps as usize)
        };
        assert!(bytes[..len] == pattern(5, len)[..], "the window maps other data");
        assert!(bytes[len..].iter().all(|byte| *byte == 0), "the padding isn't zeros");
        block_on(window.fault(5 * ps, Access::Read)).unwrap();
        // SAFETY: As above.
        let past = unsafe {
            std::slice::from_raw_parts(window.host_base().add(5 * ps as usize), ps as usize)
        };
        assert!(past.iter().all(|byte| *byte == 0), "past the extent isn't zeros");
        // Unmapped, the range has nothing to fault in, which an access's fault there is told.
        store.unmap(ps, 2 * ps).unwrap();
        assert!(!window.is_mapped(ps));
        assert_eq!(block_on(window.fault(2 * ps, Access::Read)), Err(Errno::ENOENT));
    }

    /// Races `steps` on one extent, `rounds` times, a fresh extent each: `prepare` alone first,
    /// then each step on a thread of its own, all released at once, then a commit, after which the
    /// extent reads `expected` (TASK-119.19). The steps are ones whose outcome is the same whichever
    /// comes first: what one does between another's steps is what such a race loses.
    #[allow(clippy::disallowed_methods, reason = "two threads of the test's, racing")]
    async fn race(
        store: &OpenCell,
        rounds: u64,
        first: impl AsyncFn(ExtentId) + Sync,
        second: impl AsyncFn(ExtentId) + Sync,
        expected: &[u8],
    ) {
        // Each step on a thread of its own, at once, as two of a guest's requests are: its
        // outermost entry, its awaits on the test's runtime.
        let runtime = tokio::runtime::Handle::current();
        for sequence in 1..=rounds {
            let extent = id(store, sequence);
            let all = std::sync::Barrier::new(2);
            tokio::task::block_in_place(|| {
                std::thread::scope(|scope| {
                    let (all, runtime) = (&all, &runtime);
                    scope.spawn(|| {
                        all.wait();
                        runtime.block_on(first(extent));
                    });
                    scope.spawn(|| {
                        all.wait();
                        runtime.block_on(second(extent));
                    });
                });
            });
            store.commit_ordered(None).await.unwrap();
            let read = read(store, extent, 0, expected.len()).await;
            assert_eq!(read, Ok(expected.to_vec()), "extent {sequence}");
        }
    }

    /// How many times each race is run: the race TASK-121 lost lost within a few.
    const ROUNDS: u64 = 2000;

    /// TASK-153.16: a panic holding a lock of the store's, as a lazily made resource's `expect`
    /// was in TASK-104, TASK-105 and TASK-150.20, halts the store: what it was serving, and each
    /// request after it, fails with `EIO`, rather than panicking at the lock's poison; those that
    /// take none of the locks the panic held too, so that nothing reads what it left half done.
    /// Closed, the store opens anew as of its last commit, and serves.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_panic_holding_a_lock_halts_the_store() {
        let (store, dir) = store(1 << 26).await;
        let ps = page_size();
        let window = store.create_dax_window(2 * ps, None).unwrap();
        let (sealed, open, next) = (id(&store, 1), id(&store, 2), id(&store, 3));
        write(&store, sealed, 0, &pattern(1, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        write(&store, open, 0, &pattern(2, BLOCK)).await.unwrap();

        crate::halt::tests::panic_holding("opening");
        let data = pattern(3, BLOCK);
        let writing = std::panic::AssertUnwindSafe(write(&store, open, 0, &data));
        let panicked = futures::FutureExt::catch_unwind(writing).await;
        assert!(panicked.is_err(), "the write didn't panic");
        assert!(store.is_halted());
        assert_eq!(read(&store, open, 0, BLOCK).await, Err(Errno::EIO));
        assert_eq!(read(&store, sealed, 0, BLOCK).await, Err(Errno::EIO));
        assert_eq!(write(&store, next, 0, &pattern(4, BLOCK)).await, Err(Errno::EIO));
        assert_eq!(store.free(sealed), Err(Errno::EIO));
        assert_eq!(store.commit(None).await, Err(Errno::EIO));
        // The window's, which the panic didn't hold.
        assert_eq!(store.map(sealed, 0, ps, ps).await, Err(Errno::EIO));
        assert_eq!(store.unmap(0, ps), Err(Errno::EIO));
        assert_eq!(store.window_mappings(), Err(Errno::EIO));
        // Not quiescent, as far as anyone knows: its connection's end retires it.
        assert_eq!(store.is_quiescent(), Err(Errno::EIO));
        assert_eq!(store.take_over(), Err(Errno::EIO));
        // Nor what the control API changes of it: its capacity, its versions (TASK-611).
        let committed = store.store.last_commit().unwrap();
        assert_eq!(store.set_capacity(1 << 27).await, Err(Errno::EIO));
        let dropped = store.drop_versions(&[1].into()).await.map_err(|err| err.errno());
        assert_eq!(dropped, Err(Some(Errno::EIO)));
        let shared = store.share_version(1, true).await.map_err(|err| err.errno());
        assert_eq!(shared, Err(Some(Errno::EIO)));
        assert_eq!(store.store.last_commit().unwrap(), committed, "the halted store committed");
        drop((window, store));

        let store = OpenCell::open(dir.path().join("store")).await.unwrap();
        assert!(!store.is_halted());
        assert_eq!(read(&store, sealed, 0, BLOCK).await, Ok(pattern(1, BLOCK)));
        assert_eq!(read(&store, open, 0, BLOCK).await, Err(Errno::ENOENT));
        write(&store, next, 0, &pattern(5, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        assert_eq!(read(&store, next, 0, BLOCK).await, Ok(pattern(5, BLOCK)));
    }

    /// A write to an extent not open racing a commit: committed by it, or by the next, which
    /// seals it.
    #[tokio::test(flavor = "multi_thread")]
    async fn writes_racing_a_commit_are_committed() {
        let (store, _dir) = store(1 << 26).await;
        let data = pattern(3, BLOCK);
        let write = async |extent| write(&store, extent, 0, &data).await.unwrap();
        let commit = async |_: ExtentId| store.commit_ordered(None).await.unwrap();
        race(&store, ROUNDS, write, commit, &data).await;
    }

    /// Commits ordered wait for a flush, which makes them local.
    #[tokio::test(flavor = "multi_thread")]
    async fn ordered_commits_wait_for_a_flush() {
        let (store, _dir) = store(1 << 20).await;
        write(&store, id(&store, 1), 0, &pattern(1, BLOCK)).await.unwrap();
        store.commit_ordered(None).await.unwrap();
        assert!(store.flush_pending());
        store.flush().await.unwrap();
        assert!(!store.flush_pending());
    }

    /// A read-only mapping of a sealed extent maps zeros past its last page, and only zeros if it
    /// is all past it.
    #[tokio::test(flavor = "multi_thread")]
    async fn mappings_past_a_sealed_extents_end_map_zeros() {
        let (store, _dir) = store(1 << 20).await;
        let ps = page_size();
        let sealed = id(&store, 1);
        write(&store, sealed, 0, &pattern(3, 2 * ps as usize)).await.unwrap();
        store.commit(None).await.unwrap();
        let _window = store.create_dax_window(8 * ps, None).unwrap();
        store.map(sealed, ps, 3 * ps, 0).await.unwrap();
        store.map(sealed, 2 * ps, ps, 5 * ps).await.unwrap();
        let contents = MappingContents::Extent { extent: sealed, offset: ps };
        assert_eq!(
            store.window_mappings().unwrap(),
            [
                WindowMapping { window_offset: 0, len: ps, contents },
                WindowMapping { window_offset: ps, len: 2 * ps, contents: MappingContents::Zeros },
                WindowMapping { window_offset: 5 * ps, len: ps, contents: MappingContents::Zeros },
            ]
        );
    }

    /// TASK-50.3: makes a store at `path` of `extents` sealed extents of a MiB each, and opens it
    /// in `arena`, with a window mapping them all, read-only.
    async fn mapped_extents(
        path: &Path,
        extents: u64,
        arena: &Arc<Arena>,
    ) -> (OpenCell, DaxWindow) {
        const EXTENT: u64 = 1 << 20;
        let store = OpenCell::create(path, 1 << 30).await.unwrap();
        for sequence in 1..=extents {
            write(&store, id(&store, sequence), 0, &pattern(sequence as u8, EXTENT as usize))
                .await
                .unwrap();
        }
        store.commit(None).await.unwrap();
        drop(store);
        let store = OpenCell::open_in(path, Some(arena.clone())).await.unwrap();
        let window = store.create_dax_window(extents * EXTENT, None).unwrap();
        for sequence in 1..=extents {
            store.map(id(&store, sequence), 0, EXTENT, (sequence - 1) * EXTENT).await.unwrap();
        }
        (store, window)
    }

    /// TASK-50.3: reads `extents` extents of `mapped_extents`' through `window`, a page at a time,
    /// twice over, checking their bytes, and calls `read` after each.
    async fn read_twice(window: &DaxWindow, extents: u64, read: impl Fn(u64, u64)) {
        const EXTENT: usize = 1 << 20;
        let ps = page_size();
        for round in 0..2 {
            for sequence in 1..=extents {
                let expected = pattern(sequence as u8, EXTENT);
                for offset in (0..EXTENT as u64).step_by(ps as usize) {
                    let at = (sequence - 1) * EXTENT as u64 + offset;
                    let at_page = format!("round {round}, extent {sequence}, offset {offset:#x}");
                    window.fault(at, Access::Read).await.expect(&at_page);
                    // SAFETY: The page is mapped, and nothing else accesses it.
                    let byte = unsafe { *window.host_base().add(at as usize) };
                    assert_eq!(byte, expected[offset as usize], "{at_page}");
                }
                read(round, sequence);
            }
        }
    }

    /// TASK-50.3: a store whose arena is capped reads through its window more than the cap holds:
    /// the extents asked for longest ago give their pages back, paged in again as they are touched,
    /// and the arena stays within its cap.
    #[tokio::test(flavor = "multi_thread")]
    async fn capped_arenas_evict_their_least_used_extents() {
        let chunk = 2u64 << 20;
        let dir = tempfile::tempdir().unwrap();
        let held = Arena::new();
        let arena = held.id();
        physmem::set_arena_cap(arena, Some(4 * chunk));
        let (store, window) = mapped_extents(&dir.path().join("store"), 16, &held).await;
        let first = id(&store, 1);
        // What the cache holds of extent 1.
        let cached = || cache::find(arena, first).expect("extent 1 cached").held();
        read_twice(&window, 16, |round, sequence| {
            let held = physmem::arena_bytes(arena);
            assert!(held <= 4 * chunk, "round {round}, extent {sequence}: {held}");
            // Once all were read, the one asked for longest ago gave its pages back, as its
            // first page's read in again.
            if (round, sequence) == (0, 16) {
                assert_eq!(cached(), 0, "extent 1 was not evicted");
            }
        })
        .await;
    }

    /// TASK-92.32: relief takes first what no window maps, though what one maps was used longer
    /// ago, as a guest using its mappings gives no sign of it. TASK-92.33: extents it evicted, which
    /// stay cached, are left out of the next pass.
    #[tokio::test(flavor = "multi_thread")]
    async fn relief_evicts_what_no_window_maps_first() {
        const EXTENT: u64 = 1 << 20;
        let dir = tempfile::tempdir().unwrap();
        let held = Arena::new();
        let (store, window) = mapped_extents(&dir.path().join("store"), 2, &held).await;
        let cached = |sequence| cache::find(held.id(), id(&store, sequence)).expect("cached");
        for sequence in 1..=2 {
            block_on(window.fault((sequence - 1) * EXTENT, Access::Read)).unwrap();
        }
        let (mapped, unmapped) = (cached(1).held(), cached(2).held());
        store.unmap(EXTENT, EXTENT).unwrap();

        let evicted = cache::evict_least_used(Some(held.id()), |evicted| evicted > 0);
        assert_eq!(evicted, unmapped * page_size());
        assert_eq!((cached(1).held(), cached(2).held()), (mapped, 0));
        assert_eq!(cached(2).eviction_order(), None);

        let evicted = cache::evict_least_used(Some(held.id()), |_| false);
        assert_eq!(evicted, mapped * page_size());
        assert_eq!(cached(1).eviction_order(), None);
    }

    /// TASK-92.32: of what windows map, relief takes what was faulted on longest ago, whichever
    /// its one MAP came first.
    #[tokio::test(flavor = "multi_thread")]
    async fn relief_evicts_what_was_faulted_on_longest_ago_first() {
        const EXTENT: u64 = 1 << 20;
        let dir = tempfile::tempdir().unwrap();
        let held = Arena::new();
        let (store, window) = mapped_extents(&dir.path().join("store"), 2, &held).await;
        let cached = |sequence| cache::find(held.id(), id(&store, sequence)).expect("cached");
        for sequence in [2, 1] {
            block_on(window.fault((sequence - 1) * EXTENT, Access::Read)).unwrap();
        }
        let (recent, oldest) = (cached(1).held(), cached(2).held());

        let evicted = cache::evict_least_used(Some(held.id()), |evicted| evicted > 0);
        assert_eq!(evicted, oldest * page_size());
        assert_eq!((cached(1).held(), cached(2).held()), (recent, 0));
    }

    /// TASK-50.3: physical memory run out, its arenas uncapped, evicts the extents asked for longest
    /// ago, any arena's, rather than wait for memory for ever, and abort.
    #[tokio::test(flavor = "multi_thread")]
    async fn memory_run_out_evicts_the_least_used_extents() {
        let memory = 48u64 << 20;
        crate::pages::configure(crate::pages::Config::with_memory(memory)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (_store, window) = mapped_extents(&dir.path().join("store"), 64, &Arena::new()).await;
        read_twice(&window, 64, |_, _| ()).await;
        assert_eq!(physmem::count_free_chunks(), 0, "memory never ran out");
    }

    /// A store holds a block at least; one asked for less is not made, nor its directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_hold_a_block_at_least() {
        let dir = tempfile::tempdir().unwrap();
        let block = OpenCell::BLOCK_SIZE as u64;
        let small = dir.path().join("small");
        let error = OpenCell::create(&small, block - 1).await.err().unwrap();
        assert_eq!(error.to_string(), "the store is smaller than a block");
        assert!(!host_fs::exists(&small));
        let store = OpenCell::create(dir.path().join("one"), block).await.unwrap();
        assert_eq!(store.info().unwrap().capacity, block);
    }

    /// A fork that fails but for something at its path, here as its child's store, made, fails
    /// to finish, says so, with no errno of `EEXIST`'s, and leaves nothing at its path.
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_failing_but_at_their_path_say_so() {
        let (store, dir) = store(1 << 20).await;
        let fork = dir.path().join("fork");
        let made = fail::FailGuard::new("fork::made", "return").unwrap();
        let failed = store
            .fork_to(&fork, crate::host_cell::fork_child(store.cell()))
            .await
            .expect_err("the fork was made");
        drop(made);
        assert_eq!(failed.errno(), None, "{failed:#}");
        assert!(!host_fs::exists(&fork), "the failed fork's store stays");
    }

    /// TASK-39.1: the window's mappings, as a snapshot of the VM records them once nothing is
    /// open, mapped again in a window of a fork of the store, map the same bytes at the same
    /// offsets: mappings split by later ones keep their place in their extents.
    #[tokio::test(flavor = "multi_thread")]
    async fn window_mappings_map_the_same_again_in_a_fork() {
        let (store, dir) = store(1 << 20).await;
        let ps = page_size();
        let (sealed, open) = (id(&store, 1), id(&store, 2));
        write(&store, sealed, 0, &pattern(1, 2 * ps as usize)).await.unwrap();
        store.commit(None).await.unwrap();
        let window = store.create_dax_window(8 * ps, None).unwrap();
        store.map(sealed, 0, 2 * ps, 0).await.unwrap();
        store.map(sealed, ps, ps, 4 * ps).await.unwrap();
        store.map_zeros_at(2 * ps, ps).unwrap();
        // Zeros over the first page of the first mapping: what is left of it maps the extent's
        // second page.
        store.map_zeros_at(0, ps).unwrap();
        write(&store, open, 0, &pattern(2, ps as usize)).await.unwrap();
        assert_eq!(store.is_quiescent(), Ok(false));
        assert_eq!(
            store.map(open, 0, ps, 6 * ps).await,
            Err(Errno::EPERM),
            "an open extent mapped"
        );
        store.commit(None).await.unwrap();
        assert_eq!(store.is_quiescent(), Ok(true));
        store.map(open, 0, ps, 6 * ps).await.unwrap();
        let extent = |extent, offset| MappingContents::Extent { extent, offset };
        let mappings = store.window_mappings().unwrap();
        assert_eq!(
            mappings,
            [
                WindowMapping { window_offset: 0, len: ps, contents: MappingContents::Zeros },
                WindowMapping { window_offset: ps, len: ps, contents: extent(sealed, ps) },
                WindowMapping { window_offset: 2 * ps, len: ps, contents: MappingContents::Zeros },
                WindowMapping { window_offset: 4 * ps, len: ps, contents: extent(sealed, ps) },
                WindowMapping { window_offset: 6 * ps, len: ps, contents: extent(open, 0) },
            ]
        );

        let path = dir.path().join("fork");
        store.fork_to(&path, crate::host_cell::fork_child(store.cell())).await.unwrap();
        let fork = OpenCell::open(&path).await.unwrap();
        let fork_window = fork.create_dax_window(8 * ps, None).unwrap();
        for mapping in &mappings {
            match mapping.contents {
                MappingContents::Extent { extent, offset } => {
                    fork.map(extent, offset, mapping.len, mapping.window_offset).await
                }
                MappingContents::Zeros => fork.map_zeros_at(mapping.window_offset, mapping.len),
            }
            .unwrap();
        }
        assert_eq!(fork.window_mappings().unwrap(), mappings);
        for mapping in &mappings {
            let bytes = |window: &DaxWindow| {
                block_on(window.fault(mapping.window_offset, Access::Read)).unwrap();
                // SAFETY: The test faulted the page in, and nothing else accesses it.
                unsafe {
                    std::slice::from_raw_parts(
                        window.host_base().add(mapping.window_offset as usize),
                        ps as usize,
                    )
                }
                .to_vec()
            };
            assert!(bytes(&window) == bytes(&fork_window), "{mapping:?} maps other bytes");
        }
    }

    /// cladefs: a sealed extent stored compressed pages in a part (`extent_frame::PART`) at a time,
    /// decompressing each once, however many faults read it.
    #[tokio::test(flavor = "multi_thread")]
    async fn page_ins_read_whole_parts() {
        let (store, _dir) = held(64 << 20).await;
        let ps = page_size();
        let extent = id(&store, 1);
        // Large enough to be stored on its own, compressed, in four parts.
        let len = 4 * extent_frame::PART as usize;
        write(&store, extent, 0, &pattern(5, len)).await.unwrap();
        let window = store.create_dax_window(len as u64, None).unwrap();
        committed_here(&store).await;
        store.map(extent, 0, len as u64, 0).await.unwrap();
        let before = stats::stats();
        // From its end, so that nothing is read ahead.
        block_on(async {
            let pages: Vec<u64> = (0..len as u64).step_by(ps as usize).collect();
            for page in pages.into_iter().rev() {
                window.fault(page, Access::Read).await.unwrap();
            }
        });
        let counted = stats::stats().since(&before);
        assert_eq!((counted.page_ins, counted.page_in_bytes, counted.unpacks), (4, len as u64, 4));
        // SAFETY: The test faulted the pages in, and nothing else accesses them.
        let bytes = unsafe { std::slice::from_raw_parts(window.host_base(), len) };
        assert!(*bytes == pattern(5, len)[..], "the window maps other data");
    }

    /// cladefs: page-ins of an extent sealed in a segment, at an offset that is not a part's, are of
    /// the segment's parts, each read once: four for an extent of 3 MiB in four parts.
    #[tokio::test(flavor = "multi_thread")]
    async fn page_ins_read_the_parts_extents_are_in() {
        let (store, _dir) = held(64 << 20).await;
        let ps = page_size();
        let (before, extent) = (id(&store, 1), id(&store, 2));
        write(&store, before, 0, &pattern(4, 300 << 10)).await.unwrap();
        let len = 3 * extent_frame::PART as usize;
        write(&store, extent, 0, &pattern(5, len)).await.unwrap();
        let window = store.create_dax_window(len as u64, None).unwrap();
        committed_here(&store).await;
        // Its segment closed, compressed in parts, the extent 300 KiB into the first: framed, which
        // no commit waits for (TASK-165.9).
        store.store.close_segment().unwrap();
        committed_here(&store).await;
        store.store.wait_framed().await.unwrap();
        store.map(extent, 0, len as u64, 0).await.unwrap();
        let counted = stats::stats();
        // From its end, so that nothing is read ahead.
        block_on(async {
            let pages: Vec<u64> = (0..len as u64).step_by(ps as usize).collect();
            for page in pages.into_iter().rev() {
                window.fault(page, Access::Read).await.unwrap();
            }
        });
        let counted = stats::stats().since(&counted);
        assert_eq!((counted.page_ins, counted.page_in_bytes, counted.unpacks), (4, len as u64, 4));
        // SAFETY: The test faulted the pages in, and nothing else accesses them.
        let bytes = unsafe { std::slice::from_raw_parts(window.host_base(), len) };
        assert!(*bytes == pattern(5, len)[..], "the window maps other data");
    }

    /// cladefs: a byte the host's disk damaged in a sealed extent's file, in a part stored compressed
    /// or raw, fails the page-ins of the part, and so the faults on it and the guest's reads of it,
    /// rather than mapping or giving another byte; the extent's other parts read as written
    /// (TASK-153.11).
    #[tokio::test(flavor = "multi_thread")]
    async fn flipped_bytes_fail_page_ins_and_reads() {
        let (store, dir) = held(64 << 20).await;
        let ps = page_size();
        let part = extent_frame::PART;
        // Large enough to be stored on their own, in four parts: compressed, and raw.
        let len = 4 * part as usize;
        let contents = [
            (id(&store, 1), pattern(5, len), extent_frame::Encoding::Zstd),
            (id(&store, 2), crate::test_data::noise(2, len), extent_frame::Encoding::Raw),
        ];
        for (extent, data, _) in &contents {
            write(&store, *extent, 0, data).await.unwrap();
        }
        let window = store.create_dax_window(2 * len as u64, None).unwrap();
        committed_here(&store).await;
        for (at, (extent, data, encoding)) in contents.iter().enumerate() {
            // A byte of its third part, as stored: itself if raw, else the middle of the part's.
            let path = dir.path().join(format!(
                "store/extents/{}/{}",
                base32_id::Padded(extent.cell),
                base32_id::Padded(extent.sequence)
            ));
            let mut bytes = host_fs::read(&path).unwrap();
            let parts = extent_frame::Parts::of(&bytes, &extent_frame::NoKeys).unwrap();
            let opened = parts.part(2).unwrap();
            let (place, stored) = (opened.place, opened.stored);
            assert_eq!(stored.encoding, *encoding);
            let offset = place.start + 5 * ps + 7;
            let byte = match stored.encoding {
                extent_frame::Encoding::Raw => stored.offset + (offset - place.start),
                extent_frame::Encoding::Zstd => stored.offset + stored.len / 2,
            };
            bytes[byte as usize] ^= 0x10;
            host_fs::write(&path, bytes).unwrap();
            // Damaged behind the store's back: what the store's page cache holds of it, clean, goes,
            // so that its reads read the disk (TASK-433).
            local_cell::pages::shared().evict_to(0);

            let start = at as u64 * len as u64;
            store.map(*extent, 0, len as u64, start).await.unwrap();
            let page = offset / ps * ps;
            assert_eq!(block_on(window.fault(start + page, Access::Read)), Err(Errno::EIO));
            assert_eq!(read(&store, *extent, page, BLOCK).await, Err(Errno::EIO));
            assert_eq!(read(&store, *extent, 0, len).await, Err(Errno::EIO));
            // Its first part is whole.
            block_on(window.fault(start, Access::Read)).unwrap();
            // SAFETY: The test faulted the page in, and nothing else accesses it.
            let first = unsafe {
                std::slice::from_raw_parts(window.host_base().add(start as usize), ps as usize)
            };
            assert!(*first == data[..ps as usize], "{extent:?}'s first page maps other data");
            assert_eq!(read(&store, *extent, 0, BLOCK).await, Ok(data[..BLOCK].to_vec()));
        }
    }

    /// cladefs: a reader reading an extent from its start has the parts after the one it reads read
    /// ahead of it, before it faults on them.
    #[tokio::test(flavor = "multi_thread")]
    async fn page_ins_read_ahead_of_a_reader_reading_on() {
        let (store, _dir) = held(64 << 20).await;
        let extent = id(&store, 1);
        let len = 4 * extent_frame::PART as usize;
        write(&store, extent, 0, &pattern(5, len)).await.unwrap();
        let window = store.create_dax_window(len as u64, None).unwrap();
        committed_here(&store).await;
        store.map(extent, 0, len as u64, 0).await.unwrap();
        let before = stats::stats();
        block_on(window.fault(0, Access::Read)).unwrap();
        // The first part, and the two after it, read ahead, in time.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let read = || stats::stats().since(&before).page_in_bytes;
        while read() < 3 * extent_frame::PART && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let counted = stats::stats().since(&before);
        assert_eq!((counted.page_ins, counted.page_in_bytes), (3, 3 * extent_frame::PART));
    }

    /// TASK-702.2: forks of a version opened for guests read their tenant's evidence of it, once an
    /// interval for all the forks a host opens, and the first of them in the interval records what
    /// its guest reads of the version's extents, written as its store closes; another tenant's
    /// forks read and write theirs alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_of_a_version_read_and_record_their_tenants_evidence() {
        use bucket_cell::evidence::{Prediction, UNIT, Unit};
        let (base, dir) = store(64 << 20).await;
        let extent = id(&base, 1);
        write(&base, extent, 0, &pattern(5, 3 * UNIT as usize)).await.unwrap();
        base.commit(None).await.unwrap();
        let at = dir.path().join("version");
        base.uploaded_version_at(&at).await.unwrap();
        let version = OpenCell::open_without_profile(&at).await.unwrap();
        let fork = async |name: &str, tenant| {
            let path = dir.path().join(name);
            version.fork_to(&path, host_cell::named(tenant)).await.unwrap();
            path
        };
        let (first, second, theirs) =
            (fork("first", 5).await, fork("second", 5).await, fork("theirs", 6).await);
        let open = async |path: &Path| OpenCell::open(path).await.unwrap();
        let close = async |store: OpenCell| store.close().await;
        let predicted = async |store: &OpenCell| store.predicted().unwrap().await.unwrap();

        // The first of tenant 5's forks reads none, and records its reads; the second, opened
        // within the interval, shares its prediction and records nothing.
        let (one, two) = (open(&first).await, open(&second).await);
        // Reads after the openings, in order: the first 5 ms on, the next 9.
        let now = std::time::Instant::now();
        let at = |ms| now + std::time::Duration::from_millis(ms);
        assert!(one.records_reads() && !two.records_reads());
        assert!(one.predicted().unwrap().ptr_eq(&two.predicted().unwrap()));
        assert_eq!(*predicted(&one).await, Prediction::default());
        one.guest_read(extent, 2 * UNIT, 1, at(5));
        one.guest_read(extent, 0, 4096, at(9));
        one.guest_read(id(&one, 1), 0, 4096, at(1));
        two.guest_read(extent, UNIT, 1, at(1));
        let other = open(&theirs).await;
        assert!(other.records_reads());
        other.guest_read(extent, UNIT, 1, at(1));
        let (version_cell, origin) = (base.cell(), one.forked_from());
        drop(base);
        for store in [one, two, other] {
            close(store).await;
        }

        // Each tenant reads its own evidence alone, once the host reads it again.
        crate::evidence::forget();
        let units = async |store: &OpenCell| -> Vec<(ExtentId, u64)> {
            let predicted = predicted(store).await;
            assert_eq!(predicted.recordings, 1);
            predicted.units.iter().map(|&Unit { extent, mib, .. }| (extent, mib)).collect()
        };
        let (two, other) = (open(&second).await, open(&theirs).await);
        assert_eq!(units(&two).await, [(extent, 2), (extent, 0)]);
        assert_eq!(units(&other).await, [(extent, 1)]);
        let origin = origin.unwrap().origin;
        assert_eq!(origin.parent, version_cell);
        let bucket = two.bucket.clone();
        for store in [two, other] {
            close(store).await;
        }
        close(version).await;
        let evidence = bucket_cell::layout::evidence_of_cell(version_cell);
        let listed = bucket_cell::bucket::Bucket::list(&*bucket, &evidence).await.unwrap();
        let tenants: Vec<u64> = (listed.iter())
            .map(|meta| match bucket_cell::layout::kind(&meta.key) {
                bucket_cell::layout::Kind::Evidence { tenant, at, .. } if at == origin.at => tenant,
                kind => panic!("{}: {kind:?}", meta.key),
            })
            .collect();
        assert_eq!(tenants, [5, 6]);
    }

    /// cladefs: a fork of a base records the units of the base's extents its page-ins read, as the
    /// base's boot profile, which closing it writes; the forks opened after it read them into the
    /// cache, and record nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_record_and_replay_their_bases_boot_profile() {
        let (base, dir) = store(64 << 20).await;
        let ps = page_size();
        let extent = id(&base, 1);
        let part = extent_frame::PART;
        let len = 4 * part as usize;
        write(&base, extent, 0, &pattern(5, len)).await.unwrap();
        base.commit(None).await.unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        base.fork_to(&first, crate::host_cell::fork_child(base.cell())).await.unwrap();
        base.fork_to(&second, crate::host_cell::fork_child(base.cell())).await.unwrap();
        let profile = profile::path(&dir.path().join("store"), base.cell().id);
        drop(base);

        // The first fork reads two of the extent's parts, from its end.
        let fork = OpenCell::open(&first).await.unwrap();
        let window = fork.create_dax_window(len as u64, None).unwrap();
        fork.map(extent, 0, len as u64, 0).await.unwrap();
        block_on(async {
            window.fault(3 * part + ps, Access::Read).await.unwrap();
            window.fault(part, Access::Read).await.unwrap();
        });
        assert!(!host_fs::exists(&profile));
        drop(window);
        drop(fork);
        let units = profile::read(&profile).unwrap();
        let at = |offset| profile::Unit { extent, offset, len: part };
        assert_eq!(units, [at(3 * part), at(part)]);

        // The second reads them ahead of its guest, and records nothing.
        let before = stats::stats();
        let fork = OpenCell::open(&second).await.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while stats::stats().since(&before).prefetches < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(stats::stats().since(&before).prefetches, 2);
        assert!(fork.profiling.is_none());
        drop(fork);
        assert_eq!(profile::read(&profile).unwrap(), units);
    }

    /// cladefs: a read fault on a sealed extent maps the pages its page-in supplied after it, a part
    /// of the extent, rather than a few: reading on takes no more faults.
    #[tokio::test(flavor = "multi_thread")]
    async fn read_faults_map_what_page_ins_supplied() {
        let (store, _dir) = store(64 << 20).await;
        let ps = page_size();
        let extent = id(&store, 1);
        let part = extent_frame::PART;
        let len = 4 * part as usize;
        write(&store, extent, 0, &pattern(5, len)).await.unwrap();
        let stage = Arc::new(RecordingSecondStage::default());
        let window = store.create_dax_window(len as u64, Some(stage.clone())).unwrap();
        store.commit(None).await.unwrap();
        // Nothing of it is in memory: mapping it maps no page yet.
        store.map(extent, 0, len as u64, 0).await.unwrap();
        assert_eq!(*stage.0.lock().unwrap(), []);
        // A fault on each part's first page maps the part.
        block_on(async {
            for start in (0..len as u64).step_by(part as usize) {
                window.fault(start, Access::Read).await.unwrap();
            }
        });
        let mapped: std::collections::BTreeSet<u64> = (stage.0.lock().unwrap().iter())
            .filter(|(op, ..)| *op == "map")
            .map(|(_, page)| *page)
            .collect();
        let pages: std::collections::BTreeSet<u64> = (0..len as u64).step_by(ps as usize).collect();
        assert_eq!(mapped, pages);
    }

    /// cladefs: a part paged in is pages in a run of physical memory, which a window maps with one
    /// host mapping, as the parts of an extent paged in at once are: a window of a fork maps a
    /// base's extents, paged in, as cheaply as the base's window mapped them.
    #[tokio::test(flavor = "multi_thread")]
    async fn windows_map_a_part_paged_in_with_one_mapping() {
        let (base, dir) = store(64 << 20).await;
        let extent = id(&base, 1);
        let part = extent_frame::PART;
        let parts = 8;
        let len = parts * part;
        write(&base, extent, 0, &pattern(5, len as usize)).await.unwrap();
        let window = base.create_dax_window(len, None).unwrap();
        base.commit(None).await.unwrap();
        let forked = dir.path().join("fork");
        base.fork_to(&forked, crate::host_cell::fork_child(base.cell())).await.unwrap();
        base.map(extent, 0, len, 0).await.unwrap();
        // Each part paged in by a fault of its own, at once, as a guest's threads fault.
        std::thread::scope(|scope| {
            for start in (0..len).step_by(part as usize) {
                let window = &window;
                scope.spawn(move || block_on(window.fault(start, Access::Read)).unwrap());
            }
        });
        let fork = OpenCell::open(&forked).await.unwrap();
        let stage = Arc::new(RunRecordingSecondStage::default());
        let _window = fork.create_dax_window(len, Some(stage.clone())).unwrap();
        fork.map(extent, 0, len, 0).await.unwrap();
        let runs = stage.0.lock().unwrap().clone();
        let in_one_run: Vec<bool> = (0..parts)
            .map(|at| {
                let (start, end) = (at * part, (at + 1) * part);
                runs.iter().any(|(offset, len)| *offset <= start && end <= offset + len)
            })
            .collect();
        assert_eq!(in_one_run, vec![true; parts as usize], "runs mapped: {runs:?}");
    }

    /// TASK-77: what a guest asks of its store is untrusted: any sequence of its requests, at
    /// offsets past the window's end and the extents' or at the ends of a u64, fails or doesn't,
    /// and panics nothing, and an extent it can't name reads as it was written. Seeded by
    /// proptest, which says the sequence if one fails.
    #[test]
    fn serves_whatever_a_guest_asks() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config, TestRunner};

        const WINDOW: u64 = 1 << 20;
        /// The first sequence no request names.
        const KEPT: u64 = 1 << 20;
        #[derive(Debug, Clone)]
        enum Request {
            Write(u64, u64, usize),
            Read(u64, u64, usize),
            Map(u64, u64, u64, u64),
            Unmap(u64, u64),
            Commit,
            Free(u64),
            Fork,
        }
        let at = || {
            prop_oneof![
                (0u64..64).prop_map(|page| page * BLOCK as u64),
                0u64..WINDOW,
                (0u64..4096).prop_map(|back| u64::MAX - back),
                any::<u64>(),
            ]
        };
        let sequence = || 0u64..16;
        let bytes = || 0usize..4 * BLOCK;
        // Those a store serves: of a few extents, whole blocks and pages, as a guest's are, each
        // of the others' extents, so that what one writes the next finds open, or sealed.
        let ps = page_size();
        let few = || 0u64..4;
        let page = move || (0u64..16).prop_map(move |page| page * ps);
        let request = prop_oneof![
            8 => (few(), page(), 1usize..4).prop_map(|(e, at, n)| Request::Write(e, at, n * BLOCK)),
            8 => (few(), page(), 1u64..4, page())
                .prop_map(move |(e, at, pages, to)| Request::Map(e, at, pages * ps, to)),
            8 => (sequence(), at(), bytes()).prop_map(|(e, at, len)| Request::Write(e, at, len)),
            8 => (sequence(), at(), bytes()).prop_map(|(e, at, len)| Request::Read(e, at, len)),
            8 => (sequence(), at(), at(), at())
                .prop_map(|(e, at, len, to)| Request::Map(e, at, len, to)),
            8 => (at(), at()).prop_map(|(to, len)| Request::Unmap(to, len)),
            8 => Just(Request::Commit),
            8 => sequence().prop_map(Request::Free),
            1 => Just(Request::Fork),
        ];
        let config = Config { cases: 200, failure_persistence: None, ..Config::default() };
        // Each case's requests awaited on the test's runtime, its outermost entry.
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        TestRunner::new(config)
            .run(&proptest::collection::vec(request, 1..64), |requests| {
                runtime.block_on(async {
                    // Room for every extent it may open, so that nothing it did may fail a commit.
                    let (store, dir) = store(1 << 27).await;
                    let stage = Arc::new(RunRecordingSecondStage::default());
                    let _window = store.create_dax_window(WINDOW, Some(stage)).unwrap();
                    let kept = id(&store, KEPT);
                    write(&store, kept, 0, &pattern(9, BLOCK)).await.unwrap();
                    store.commit(None).await.unwrap();
                    let mut forks = (1u64..).map(|fork| (dir.path().join(fork.to_string()), fork));
                    for request in requests {
                        let extent = |sequence| id(&store, sequence);
                        // Failing or not as it may: what it may not do is panic.
                        let _ = match request {
                            Request::Write(e, at, len) => {
                                write(&store, extent(e), at, &vec![1; len]).await
                            }
                            Request::Read(e, at, len) => {
                                read(&store, extent(e), at, len).await.map(drop)
                            }
                            Request::Map(e, at, len, to) => store.map(extent(e), at, len, to).await,
                            Request::Unmap(to, len) => store.unmap(to, len),
                            Request::Commit => store.commit(None).await,
                            Request::Free(e) => store.free(extent(e)),
                            Request::Fork => {
                                let (path, fork) = forks.next().unwrap();
                                store
                                    .fork_to(
                                        path,
                                        crate::CellId {
                                            id: store.cell().id + fork,
                                            ..store.cell()
                                        },
                                    )
                                    .await
                                    .map_err(|_| Errno::EIO)
                            }
                        };
                    }
                    // Nor leave a commit it can't make: a wedged store's guest gets nothing more done.
                    prop_assert_eq!(store.commit(None).await, Ok(()));
                    prop_assert_eq!(read(&store, kept, 0, BLOCK).await, Ok(pattern(9, BLOCK)));
                    Ok(())
                })
            })
            .unwrap();
    }

    /// A second stage recording the runs it maps: their offsets and lengths.
    #[derive(Default)]
    struct RunRecordingSecondStage(std::sync::Mutex<Vec<(u64, u64)>>);

    impl SecondStage for RunRecordingSecondStage {
        fn map(&self, offset: u64, _: PhysLocation, len: u64) {
            self.0.lock().unwrap().push((offset, len));
        }

        fn unmap(&self, _: u64, _: u64) {}
    }

    /// cladefs: a store of a cell kept in object storage, opened where its data is not, maps the
    /// cell's extents: their pages are fetched as the window's faults page them in.
    #[tokio::test(flavor = "multi_thread")]
    async fn maps_extents_whose_data_is_in_object_storage() {
        let dir = tempfile::tempdir().unwrap();
        let ps = page_size();
        let len = ps as usize + BLOCK;
        let url = format!("memory:///maps-bucket-{:016x}", rand::random::<u64>());
        let first =
            OpenCell::create_kept_in(dir.path().join("first"), 1 << 20, &url).await.unwrap();
        let extent = id(&first, 1);
        write(&first, extent, 0, &pattern(7, len)).await.unwrap();
        first.commit(None).await.unwrap();
        let cell = first.cell();
        drop(first);

        let opening = Opening::here().unwrap();
        let store = OpenCell::create_kept_with(
            dir.path().join("second"),
            1 << 20,
            cell,
            &url,
            opening.keeping,
        )
        .await;
        let store = store.unwrap();
        assert_eq!(read(&store, extent, 0, len).await, Ok(pattern(7, len)));
        let window = store.create_dax_window(4 * ps, None).unwrap();
        store.map(extent, 0, 2 * ps, ps).await.unwrap();
        block_on(async {
            window.fault(ps, Access::Read).await.unwrap();
            window.fault(2 * ps, Access::Read).await.unwrap();
        });
        // SAFETY: The test faulted the pages in, and nothing else accesses them.
        let bytes = unsafe {
            std::slice::from_raw_parts(window.host_base().add(ps as usize), 2 * ps as usize)
        };
        assert!(bytes[..len] == pattern(7, len)[..], "the window maps other data");
        assert!(bytes[len..].iter().all(|byte| *byte == 0), "the padding isn't zeros");
    }

    /// A second stage recording the changes made to it, page by page: each, and the page.
    #[derive(Default)]
    struct RecordingSecondStage(std::sync::Mutex<Vec<(&'static str, u64)>>);

    impl SecondStage for RecordingSecondStage {
        fn map(&self, offset: u64, _: PhysLocation, len: u64) {
            let mut ops = self.0.lock().unwrap();
            for page in (0..len).step_by(page_size() as usize) {
                ops.push(("map", offset + page));
            }
        }

        fn unmap(&self, offset: u64, len: u64) {
            let mut ops = self.0.lock().unwrap();
            for page in (0..len).step_by(page_size() as usize) {
                ops.push(("unmap", offset + page));
            }
        }
    }

    /// The hypervisor's second stage follows the window: a sealed extent's pages are mapped as
    /// the guest faults on them, or as they are mapped, if in memory; unmapping unmaps them; a
    /// commit changes nothing there.
    #[tokio::test(flavor = "multi_thread")]
    async fn second_stage_follows_the_window() {
        let (store, _dir) = store(1 << 20).await;
        let ps = page_size();
        let stage = Arc::new(RecordingSecondStage::default());
        let window = store.create_dax_window(4 * ps, Some(stage.clone())).unwrap();
        let take = || std::mem::take(&mut *stage.0.lock().unwrap());
        let extent = id(&store, 1);
        write(&store, extent, 0, &pattern(3, 2 * ps as usize)).await.unwrap();
        store.commit(None).await.unwrap();
        store.map(extent, 0, 2 * ps, 0).await.unwrap();
        assert_eq!(take(), [], "pages not in memory mapped");
        block_on(window.fault(ps, Access::Read)).unwrap();
        assert_eq!(take(), [("map", ps)]);
        // The extent's first page, paged in with the second, is mapped as the range is.
        store.map(extent, 0, ps, 3 * ps).await.unwrap();
        assert_eq!(take(), [("map", 3 * ps)]);
        write(&store, id(&store, 2), 0, &pattern(4, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        assert_eq!(take(), [], "a commit changed the window");
        store.unmap(0, 2 * ps).unwrap();
        assert_eq!(take(), [("unmap", ps)]);
    }

    /// TASK-79.45: a writer taking a store over maps zeros through a window of its own, however
    /// much larger than the window of the writer before it: zeros as many as its own window's.
    #[tokio::test(flavor = "multi_thread")]
    async fn writers_taking_over_map_zeros_across_windows_of_their_own() {
        let (store, _dir) = store(1 << 20).await;
        let ps = page_size();
        let _window = store.create_dax_window(2 * ps, None).unwrap();
        store.map_zeros_at(0, ps).unwrap();
        store.take_over().unwrap();
        let window = store.create_dax_window(8 * ps, None).unwrap();
        store.map_zeros_at(6 * ps, 2 * ps).unwrap();
        block_on(window.fault(7 * ps, Access::Read)).unwrap();
        // SAFETY: The test faulted the page in, and nothing else accesses it.
        let page = unsafe {
            std::slice::from_raw_parts(window.host_base().add(7 * ps as usize), ps as usize)
        };
        assert!(page.iter().all(|byte| *byte == 0), "the window's last page isn't zeros");
    }

    /// cladefs: freeing an extent maps zeros where the window mapped it, read-only, rather than
    /// what kept its pages; others stay.
    #[tokio::test(flavor = "multi_thread")]
    async fn freeing_an_extent_maps_zeros_in_its_place() {
        let (store, _dir) = store(1 << 20).await;
        let ps = page_size();
        let window = store.create_dax_window(8 * ps, None).unwrap();
        let (sealed, kept) = (id(&store, 1), id(&store, 2));
        write(&store, sealed, 0, &pattern(1, 2 * ps as usize)).await.unwrap();
        write(&store, kept, 0, &pattern(2, ps as usize)).await.unwrap();
        store.commit(None).await.unwrap();
        store.map(sealed, 0, 2 * ps, 0).await.unwrap();
        store.map(kept, 0, ps, 4 * ps).await.unwrap();
        assert!(window.is_mapped(0) && window.is_mapped(ps));

        let sealed_pages = cached(sealed).unwrap();
        let weak = Arc::downgrade(&sealed_pages);
        drop(sealed_pages);
        store.free(sealed).unwrap();
        // Mapped, but no longer keeping the extent's pages: the sealed extent is gone.
        assert!(window.is_mapped(0) && window.is_mapped(ps));
        assert!(weak.upgrade().is_none(), "the window still keeps the freed extent's pages");
        assert!(cached(sealed).is_none());
        // Zeros, read-only: a store faults rather than writing them.
        assert_eq!(block_on(window.fault(0, Access::Write)), Err(Errno::EACCES));
        block_on(window.fault(0, Access::Read)).unwrap();
        // SAFETY: The test faulted the pages in, and nothing else accesses them.
        let bytes = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
        assert!(bytes.iter().all(|byte| *byte == 0), "a freed extent's range doesn't read zeros");
        assert!(window.is_mapped(4 * ps), "an extent not freed was unmapped");
    }

    /// A window in host memory takes whole pages.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn dax_window_in_host_memory_must_be_whole_pages() {
        let (store, _dir) = store(1 << 20).await;
        let ps = page_size() as usize;
        let mut memory = vec![0u8; 3 * ps];
        let aligned = memory.as_mut_ptr().wrapping_add(ps - memory.as_ptr() as usize % ps);
        let base = std::ptr::NonNull::new(aligned).unwrap();
        // SAFETY: Fails before using the memory.
        let result = unsafe { store.create_dax_window_in_host_memory(base, ps as u64 + 1, None) };
        let error = result.expect_err("succeeded").to_string();
        assert_eq!(error, "DAX window memory must be whole pages");
    }

    /// Windows mapping the same extent, from a store and a fork of it, map the same pages.
    #[tokio::test(flavor = "multi_thread")]
    async fn windows_share_extents_pages() {
        let (store, dir) = store(1 << 20).await;
        let ps = page_size();
        let extent = id(&store, 1);
        write(&store, extent, 0, &pattern(6, ps as usize)).await.unwrap();
        store.commit(None).await.unwrap();
        let copy_path = dir.path().join("copy");
        store.fork_to(&copy_path, crate::host_cell::fork_child(store.cell())).await.unwrap();
        let copy = OpenCell::open(&copy_path).await.unwrap();
        let (window, copy_window) =
            (store.create_dax_window(ps, None).unwrap(), copy.create_dax_window(ps, None).unwrap());
        store.map(extent, 0, ps, 0).await.unwrap();
        copy.map(extent, 0, ps, 0).await.unwrap();

        // Counted before anything pages in: a page-in holds the extent too, a while after the
        // fault it serves returns.
        let extent_cached = cached(extent).unwrap();
        // Each window's mapping holds the one copy of the extent's pages, which the cache keeps.
        assert_eq!(Arc::strong_count(&extent_cached), 4);
        copy.unmap(0, ps).unwrap();
        assert_eq!(Arc::strong_count(&extent_cached), 3, "unmapped, the mapping let go");
        copy.map(extent, 0, ps, 0).await.unwrap();
        block_on(async {
            window.fault(0, Access::Read).await.unwrap();
            copy_window.fault(0, Access::Read).await.unwrap();
        });
        for window in [&window, &copy_window] {
            // SAFETY: The test faulted the page in, and nothing writes it.
            let mapped = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
            assert!(mapped == pattern(6, ps as usize), "a window maps other data");
        }
        store.unmap(0, ps).unwrap();
        copy.unmap(0, ps).unwrap();
        // Unmapped, it stays cached, for the next read or mapping, until the kernel evicts its
        // pages.
        assert!(Arc::ptr_eq(&cached(extent).unwrap(), &extent_cached));
        // cladefs: freed by one store, it stays cached for the other, which holds it still
        // (TASK-79.7); freed by that one too, it is no longer cached.
        store.free(extent).unwrap();
        assert!(Arc::ptr_eq(&cached(extent).unwrap(), &extent_cached));
        assert_eq!(read(&copy, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));
        drop(extent_cached);
        copy.free(extent).unwrap();
        assert!(cached(extent).is_none());
    }

    /// TASK-79.6: an extent freed once sealed is mapped, and written, no more: a fork
    /// that shares its pages through the cache, by its ID, would read what its writer stored in
    /// them as its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn freed_extents_are_written_no_more() {
        let (store, dir) = store(1 << 20).await;
        let ps = page_size();
        let extent = id(&store, 1);
        write(&store, extent, 0, &pattern(6, ps as usize)).await.unwrap();
        store.commit(None).await.unwrap();
        let fork_path = dir.path().join("fork");
        store.fork_to(&fork_path, crate::host_cell::fork_child(store.cell())).await.unwrap();
        let fork = OpenCell::open(&fork_path).await.unwrap();
        assert_eq!(read(&fork, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));

        store.free(extent).unwrap();
        store.commit(None).await.unwrap();
        let window = store.create_dax_window(ps, None).unwrap();
        assert_eq!(store.map(extent, 0, ps, 0).await, Err(Errno::ENOENT));
        assert_eq!(write(&store, extent, 0, &pattern(7, BLOCK)).await, Err(Errno::EPERM));
        drop(window);
        assert_eq!(read(&fork, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));
    }

    /// TASK-79.7: an extent forks share in the cache pages in through any of them that holds it:
    /// one that frees it, the one it was cached through, leaves the others' mappings of it paging
    /// in through theirs.
    #[tokio::test(flavor = "multi_thread")]
    async fn forks_page_in_what_a_sibling_freed() {
        let (base, dir) = store(1 << 20).await;
        let ps = page_size();
        let extent = id(&base, 1);
        write(&base, extent, 0, &pattern(6, ps as usize)).await.unwrap();
        base.commit(None).await.unwrap();
        let mut forks: Vec<OpenCell> = Vec::new();
        for fork in 1..=2 {
            let path = dir.path().join(format!("fork-{fork}"));
            base.fork_to(&path, crate::host_cell::fork_child(base.cell())).await.unwrap();
            forks.push(OpenCell::open(&path).await.unwrap());
        }
        drop(base);
        let windows: Vec<DaxWindow> =
            forks.iter().map(|fork| fork.create_dax_window(ps, None).unwrap()).collect();
        for fork in &forks {
            fork.map(extent, 0, ps, 0).await.unwrap();
        }
        forks[0].free(extent).unwrap();
        forks[0].commit(None).await.unwrap();
        block_on(windows[1].fault(0, Access::Read)).unwrap();
        // SAFETY: The test faulted the page in, and nothing writes it.
        let mapped = unsafe { std::slice::from_raw_parts(windows[1].host_base(), ps as usize) };
        assert!(mapped == pattern(6, ps as usize), "the window maps other data");
    }

    /// TASK-79.7: the cache keeps no store it read an extent through once it closes, nor its
    /// files and disk space: with hundreds of forks, the daemon's file descriptors ran out.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_cache_keeps_no_store_closed() {
        let (store, _dir) = store(1 << 20).await;
        let ps = page_size();
        let extent = id(&store, 1);
        write(&store, extent, 0, &pattern(6, ps as usize)).await.unwrap();
        store.commit(None).await.unwrap();
        assert_eq!(read(&store, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));
        assert!(cached(extent).is_some());
        let closed = Arc::downgrade(&store.store);
        drop(store);
        assert!(closed.upgrade().is_none(), "the cache keeps the store");
    }

    /// cladefs (backlog doc-13): a `MAP` of an open extent, written and not yet sealed, is refused
    /// (`EPERM`), and counted, the window as it was: the guest maps sealed extents alone. Sealed
    /// by a commit, it maps, zeros past what was written; one never written maps nothing
    /// (`ENOENT`).
    #[tokio::test(flavor = "multi_thread")]
    async fn open_extents_map_nowhere() {
        let (store, _dir) = store(1 << 20).await;
        let ps = page_size();
        let window = store.create_dax_window(2 * ps, None).unwrap();
        let written = id(&store, 1);
        write(&store, written, 0, &pattern(1, ps as usize)).await.unwrap();
        let refused = open_maps_refused();
        assert_eq!(store.map(written, 0, ps, 0).await, Err(Errno::EPERM));
        assert_eq!(open_maps_refused(), refused + 1);
        assert!(!window.is_mapped(0), "an open extent mapped");
        assert_eq!(store.map(id(&store, 3), 0, ps, 0).await, Err(Errno::ENOENT));

        store.commit(None).await.unwrap();
        store.map(written, 0, 2 * ps, 0).await.unwrap();
        block_on(async {
            for page in 0..2 {
                window.fault(page * ps, Access::Read).await.unwrap();
            }
        });
        // SAFETY: The test faulted the pages in, and nothing else accesses them.
        let bytes = unsafe { std::slice::from_raw_parts(window.host_base(), 2 * ps as usize) };
        assert!(bytes[..ps as usize] == pattern(1, ps as usize)[..], "the sealed written data");
        assert!(bytes[ps as usize..].iter().all(|byte| *byte == 0), "past the sealed extent");
    }

    /// TASK-104: the cache keeps no more extents than its bound, letting go of those asked for
    /// longest ago that nothing else holds, and keeps those a window maps: under pgbench, extents
    /// by the hundred thousand, those never asked for again would be most of what it holds.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_cache_keeps_its_bound() {
        cache::MOST_CACHED.get().store(8, Ordering::Relaxed);
        let (store, _dir) = store(1 << 20).await;
        let ps = page_size();
        for sequence in 1..=20 {
            write(&store, id(&store, sequence), 0, &pattern(sequence as u8, ps as usize))
                .await
                .unwrap();
        }
        store.commit(None).await.unwrap();
        let window = store.create_dax_window(ps, None).unwrap();
        store.map(id(&store, 1), 0, ps, 0).await.unwrap();
        for sequence in 2..=20 {
            let extent = id(&store, sequence);
            let cached_in = CachedIn::Arena(ArenaId::DEFAULT);
            let cached = cache::cached(&store.store, cached_in, None, None, extent, ps);
            drop(cached.await.unwrap());
        }
        let mut kept: Vec<u64> = cache::keys().iter().map(|(_, extent)| extent.sequence).collect();
        kept.sort_unstable();
        assert_eq!(kept, [1, 14, 15, 16, 17, 18, 19, 20]);
        drop(window);
    }

    /// TASK-109.2: past its bound, with more extents than the bound held by a window's mappings,
    /// which it can't let go of, the cache looks through its extents again only once it grows by
    /// an eighth of its bound: it did at every lookup, each a sort of every extent cached.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_cache_trims_what_windows_hold_once_per_eighth_of_its_bound() {
        const MOST: usize = 64;
        cache::MOST_CACHED.get().store(MOST, Ordering::Relaxed);
        let (store, _dir) = store(8 << 20).await;
        let ps = page_size();
        let (mapped, looked_up) = (2 * MOST as u64, 4 * MOST as u64);
        for sequence in 1..=mapped + looked_up {
            write(&store, id(&store, sequence), 0, &pattern(sequence as u8, ps as usize))
                .await
                .unwrap();
        }
        store.commit(None).await.unwrap();
        let window = store.create_dax_window(mapped * ps, None).unwrap();
        for sequence in 1..=mapped {
            store.map(id(&store, sequence), 0, ps, (sequence - 1) * ps).await.unwrap();
        }
        let before = stats::stats();
        for sequence in mapped + 1..=mapped + looked_up {
            let extent = id(&store, sequence);
            let cached_in = CachedIn::Arena(ArenaId::DEFAULT);
            let cached = cache::cached(&store.store, cached_in, None, None, extent, ps);
            drop(cached.await.unwrap());
        }
        let trims = stats::stats().since(&before).cache_trims;
        assert!(trims <= looked_up / (MOST as u64 / 8) + 1, "{trims} trims of the cache");
        let cached = cache::keys().len() as u64;
        assert!(cached <= mapped + MOST as u64 / 8, "{cached} extents cached");
        drop(window);
    }

    /// The process's cached sealed `extent`, if it is cached.
    fn cached(extent: ExtentId) -> Option<Arc<CachedExtent>> {
        cache::find(ArenaId::DEFAULT, extent)
    }

    /// What one page-in reads: a part.
    const READ_AHEAD: u64 = extent_frame::PART;

    /// A source whose reads all wait until `readers` of them are waiting at once, then fill the
    /// buffer with the `pattern` of their chunk.
    struct Held {
        readers: usize,
        /// The reads waiting, and their wakers.
        waiting: Mutex<(usize, Vec<std::task::Waker>)>,
        /// Notified when all are.
        all: std::sync::Condvar,
    }

    struct HeldSource(Arc<Held>);

    impl cache::Contents for HeldSource {
        fn read<'a>(
            &'a self,
            offset: u64,
            buf: &'a mut [u8],
        ) -> futures::future::BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
            let held = &self.0;
            let mut counted = false;
            Box::pin(async move {
                std::future::poll_fn(|context| {
                    let mut waiting = held.waiting.lock().unwrap();
                    if !counted {
                        counted = true;
                        waiting.0 += 1;
                        if waiting.0 == held.readers {
                            waiting.1.drain(..).for_each(std::task::Waker::wake);
                            held.all.notify_all();
                        }
                    }
                    if waiting.0 >= held.readers {
                        return std::task::Poll::Ready(());
                    }
                    waiting.1.push(context.waker().clone());
                    std::task::Poll::Pending
                })
                .await;
                buf.copy_from_slice(&pattern((offset / READ_AHEAD) as u8, buf.len()));
                Ok(())
            })
        }
    }

    /// cladefs (TASK-28.8): page-ins waiting for their extents' reads hold no thread: more reads
    /// wait at once than the pager has threads, and all go on.
    #[tokio::test(flavor = "multi_thread")]
    async fn page_ins_wait_holding_no_thread() {
        let readers = 4 * cache::PAGER_THREADS;
        let held = Arc::new(Held {
            readers,
            waiting: Mutex::new((0, Vec::new())),
            all: std::sync::Condvar::new(),
        });
        let extent = Arc::new(CachedExtent::new(
            ArenaId::DEFAULT,
            ExtentId { cell: u64::MAX, sequence: u64::MAX },
            Box::new(HeldSource(held.clone())),
            readers as u64 * READ_AHEAD,
        ));
        let faults: Vec<_> = (0..readers as u64)
            .map(|chunk| {
                let extent = extent.clone();
                tokio::spawn(async move {
                    let mut data = vec![0; BLOCK];
                    extent.read(chunk * READ_AHEAD, &mut data).await.unwrap();
                    data
                })
            })
            .collect();
        // Were page-ins to hold their threads while they waited, no more than the pager's would.
        let waiting = held.waiting.lock().unwrap();
        let (waiting, timeout) = held
            .all
            .wait_timeout_while(waiting, std::time::Duration::from_secs(30), |waiting| {
                waiting.0 < readers
            })
            .unwrap();
        assert!(!timeout.timed_out(), "only {} of {readers} reads waited at once", waiting.0);
        drop(waiting);
        for (chunk, fault) in faults.into_iter().enumerate() {
            assert_eq!(fault.await.unwrap(), pattern(chunk as u8, BLOCK));
        }
    }

    /// A source whose reads fail with `cause`.
    struct FailingSource(&'static str);

    impl cache::Contents for FailingSource {
        fn read<'a>(
            &'a self,
            _offset: u64,
            _buf: &'a mut [u8],
        ) -> futures::future::BoxFuture<'a, std::result::Result<(), anyhow::Error>> {
            let cause = self.0;
            Box::pin(async move { Err(anyhow::anyhow!(cause).context("fetching a part")) })
        }
    }

    /// What this process logs, kept: for tests of what a failure says.
    struct KeptLog(Mutex<Vec<String>>);

    impl log::Log for KeptLog {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            self.0.lock().unwrap().push(record.args().to_string());
        }

        fn flush(&self) {}
    }

    /// cladefs: a page-in that fails says why, where the daemon logs it: what its read failed with,
    /// not EIO alone, which was all a soak's checking daemon said of an extent of a store its
    /// index placed in the bucket and that could not be read (TASK-217, GCE r757 round 40).
    #[tokio::test(flavor = "multi_thread")]
    async fn failed_page_ins_say_why() {
        static KEPT: KeptLog = KeptLog(Mutex::new(Vec::new()));
        log::set_logger(&KEPT).unwrap();
        log::set_max_level(log::LevelFilter::Error);
        let extent = Arc::new(CachedExtent::new(
            ArenaId::DEFAULT,
            ExtentId { cell: 7, sequence: 9 },
            Box::new(FailingSource("no such object at segments/7/1")),
            READ_AHEAD,
        ));
        let mut data = vec![0; BLOCK];
        assert_eq!(extent.read(0, &mut data).await, Err(Errno::EIO));
        let logged = KEPT.0.lock().unwrap().clone();
        assert!(
            logged
                .iter()
                .any(|line| line.contains("fetching a part: no such object at segments/7/1")),
            "{logged:?}"
        );
    }

    /// A store holding `data` in one sealed extent, and the generation at `path` that pins it: the
    /// extent, and the store's ancestry as the generation has it.
    async fn generation(path: &Path, data: &[u8]) -> (ExtentId, Vec<u64>) {
        let publisher = made(&path.with_extension("publisher"), 1 << 20).await;
        let extent = id(&publisher, 1);
        write(&publisher, extent, 0, data).await.unwrap();
        publisher.commit(None).await.unwrap();
        // A cell of its own, as a cell has one cell (doc-20).
        publisher.fork_to(path, crate::host_cell::fork_child(publisher.cell())).await.unwrap();
        let ancestry = OpenCell::open(path).await.unwrap().ancestry();
        (extent, ancestry)
    }

    /// What opens what stores attach, one instance of each, as the extent daemon does.
    fn one_instance_each() -> OpenBase {
        let open: Arc<tokio::sync::Mutex<HashMap<std::path::PathBuf, Arc<OpenCell>>>> =
            Arc::default();
        Arc::new(move |path| {
            let (open, path) = (open.clone(), path.to_owned());
            Box::pin(async move {
                let mut open = open.lock().await;
                if let Some(store) = open.get(&path) {
                    return Ok(store.clone());
                }
                let store = Arc::new(OpenCell::open_base(&path, None).await.unwrap());
                open.insert(path, store.clone());
                Ok(store)
            })
        })
    }

    /// doc-10: a base attached reads and maps through, read-only, and nothing else does: not an
    /// extent of a base not attached, nor one a base doesn't hold.
    #[tokio::test(flavor = "multi_thread")]
    async fn bases_attached_read_and_map_through() {
        let (store, dir) = store(1 << 20).await;
        let ps = page_size();
        let (base, other) = (dir.path().join("base"), dir.path().join("other"));
        let data = pattern(3, ps as usize);
        let (extent, ancestry) = generation(&base, &data).await;
        let (not_attached, _) = generation(&other, &data).await;
        let window = store.create_dax_window(4 * ps, None).unwrap();

        assert_eq!(read(&store, extent, 0, BLOCK).await, Err(Errno::ENOENT));
        assert_eq!(store.map(extent, 0, ps, 0).await, Err(Errno::ENOENT));

        store.attach_base(&base, &ancestry).unwrap();
        assert_eq!(read(&store, extent, 0, ps as usize).await, Ok(data.clone()));
        store.map(extent, 0, ps, 0).await.unwrap();
        block_on(window.fault(0, Access::Read)).unwrap();
        // SAFETY: The test faulted the page in, and nothing writes it.
        let mapped = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
        assert!(mapped == data, "the window maps other data");

        assert_eq!(read(&store, not_attached, 0, BLOCK).await, Err(Errno::ENOENT));
        assert_eq!(store.map(not_attached, 0, ps, ps).await, Err(Errno::ENOENT));
        let beyond = ExtentId { sequence: extent.sequence + 1, ..extent };
        assert_eq!(read(&store, beyond, 0, BLOCK).await, Err(Errno::ENOENT));
        // A base's extent maps read-only anywhere, as every MAP does; it is never written.
        store.map(extent, 0, ps, ps).await.unwrap();
        assert_eq!(write(&store, extent, 0, &[0; BLOCK]).await, Err(Errno::EPERM));
        // Attaching it again changes nothing.
        store.attach_base(&base, &ancestry).unwrap();
        assert_eq!(cell_dir::attached(&dir.path().join("store")).unwrap().len(), 1);
    }

    /// doc-10: VMs mapping a base share its pages: the cache's one copy, for the attached and for
    /// a store that holds the extent itself.
    #[tokio::test(flavor = "multi_thread")]
    async fn windows_share_bases_pages() {
        let dir = tempfile::tempdir().unwrap();
        let ps = page_size();
        let base = dir.path().join("base");
        let (extent, ancestry) = generation(&base, &pattern(4, ps as usize)).await;
        let mut stores: Vec<OpenCell> = Vec::new();
        for at in 0..2 {
            stores.push(OpenCell::create(dir.path().join(format!("{at}")), 1 << 20).await.unwrap());
        }
        let windows: Vec<DaxWindow> =
            stores.iter().map(|store| store.create_dax_window(ps, None).unwrap()).collect();
        let open = one_instance_each();
        for store in &stores {
            store.open_bases(open.clone()).unwrap();
            store.attach_base(&base, &ancestry).unwrap();
            store.map(extent, 0, ps, 0).await.unwrap();
        }
        // Counted before anything pages in: a page-in holds the extent too, a while after the
        // fault it serves returns (TASK-157), as `windows_share_extents_pages` counts.
        let extent_cached = cached(extent).expect("the extent is cached");
        // The cache's, the test's, and each window's mapping.
        assert_eq!(Arc::strong_count(&extent_cached), 4);
        for window in &windows {
            block_on(window.fault(0, Access::Read)).unwrap();
            // SAFETY: The test faulted the page in, and nothing writes it.
            let mapped = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
            assert!(mapped == pattern(4, ps as usize), "a window maps other data");
        }
        assert!(Arc::ptr_eq(&cached(extent).unwrap(), &extent_cached), "cached anew");
        let mut held = vec![0; ps as usize];
        extent_cached.read(0, &mut held).await.unwrap();
        assert!(held == pattern(4, ps as usize), "the cache holds other data");
    }

    /// TASK-234: an extent of a base its publisher's store, open in its tenant's arena, has cached
    /// is read into the default arena, which a base's pages are of, from those pages, not paged in
    /// again, and they go: one copy, the default arena's, which the publisher's store reads from
    /// then on. The first sandbox on a generation of the Linux tree paged 1.8 GB in, 14.5 s, the
    /// daemon holding it as the publisher's store's (GCE r941).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bases_extent_its_publisher_has_cached_moves_to_the_default_arena() {
        let dir = tempfile::tempdir().unwrap();
        let ps = page_size();
        let len = 4 * extent_frame::PART as usize;
        let data = pattern(7, len);
        let (publisher_path, base) = (dir.path().join("publisher"), dir.path().join("base"));
        let publisher = made(&publisher_path, 1 << 24).await;
        let extent = id(&publisher, 1);
        write(&publisher, extent, 0, &data).await.unwrap();
        publisher.commit(None).await.unwrap();
        publisher.fork_to(&base, crate::host_cell::fork_child(publisher.cell())).await.unwrap();
        drop(publisher);
        OpenCell::open(&base).await.unwrap().make_public_attach_only().unwrap();
        let tenant = Arena::new();
        let publisher = OpenCell::open_in(&publisher_path, Some(tenant.clone())).await.unwrap();
        // The publisher's VM maps its first page, from the tenant's copy.
        let window = publisher.create_dax_window(len as u64, None).unwrap();
        publisher.map(extent, 0, len as u64, 0).await.unwrap();
        block_on(window.fault(0, Access::Read)).unwrap();
        assert_eq!(read(&publisher, extent, 0, len).await, Ok(data.clone()));
        let pages = len as u64 / ps;
        let held = |arena| cache::find(arena, extent).map(|cached| cached.held());
        assert_eq!(held(tenant.id()), Some(pages));

        let ancestry = OpenCell::open(&base).await.unwrap().ancestry();
        let sandbox = made(&dir.path().join("sandbox"), 1 << 20).await;
        sandbox.open_bases(one_instance_each()).unwrap();
        sandbox.attach_base(&base, &ancestry).unwrap();
        let sandbox_window = sandbox.create_dax_window(len as u64, None).unwrap();
        let page_ins = stats::stats().page_ins;
        // Moved as the sandbox maps it, so that its MAP maps every page, none faulted on: moved
        // as asked for, the first sandbox on the Linux tree took 4,422 faults (GCE r991).
        sandbox.map(extent, 0, len as u64, 0).await.unwrap();
        assert_eq!((held(ArenaId::DEFAULT), held(tenant.id())), (Some(pages), None));
        assert_eq!(read(&sandbox, extent, 0, len).await, Ok(data.clone()));
        assert_eq!(stats::stats().page_ins - page_ins, 0, "paged in again");
        drop(sandbox_window);

        assert_eq!(read(&publisher, extent, 0, len).await, Ok(data.clone()));
        // Its window, the tenant's page evicted from it, faults onto the default arena's.
        block_on(window.fault(0, Access::Read)).unwrap();
        // SAFETY: The test faulted the page in, and nothing writes it.
        let mapped = unsafe { std::slice::from_raw_parts(window.host_base(), ps as usize) };
        assert!(mapped == &data[..ps as usize], "the publisher's window maps other data");
        assert_eq!(stats::stats().page_ins - page_ins, 0, "the publisher paged in again");
        let copies = cache::keys().into_iter().filter(|(_, cached)| *cached == extent).count();
        assert_eq!(copies, 1);
    }

    /// TASK-234: a tenant's copy of an extent moves to the default arena only for a read through a
    /// published generation: one not published, read without an arena as a base is, takes nothing
    /// of the tenant's, which keeps its copy, and pages in its own, as every VM process maps the
    /// default arena.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_extent_of_no_published_generation_stays_its_tenants() {
        let dir = tempfile::tempdir().unwrap();
        let ps = page_size();
        let len = 4 * extent_frame::PART as usize;
        let data = pattern(8, len);
        let (publisher_path, base) = (dir.path().join("publisher"), dir.path().join("base"));
        let publisher = made(&publisher_path, 1 << 24).await;
        let extent = id(&publisher, 1);
        write(&publisher, extent, 0, &data).await.unwrap();
        publisher.commit(None).await.unwrap();
        publisher.fork_to(&base, crate::host_cell::fork_child(publisher.cell())).await.unwrap();
        drop(publisher);
        let tenant = Arena::new();
        let publisher = OpenCell::open_in(&publisher_path, Some(tenant.clone())).await.unwrap();
        assert_eq!(read(&publisher, extent, 0, len).await, Ok(data.clone()));
        let pages = len as u64 / ps;
        let held = |arena| cache::find(arena, extent).map(|cached| cached.held());

        let ancestry = OpenCell::open(&base).await.unwrap().ancestry();
        let sandbox = made(&dir.path().join("sandbox"), 1 << 20).await;
        sandbox.open_bases(one_instance_each()).unwrap();
        sandbox.attach_base(&base, &ancestry).unwrap();
        let page_ins = stats::stats().page_ins;
        assert_eq!(read(&sandbox, extent, 0, len).await, Ok(data));
        assert_eq!(stats::stats().page_ins - page_ins, 4, "one page-in a part");
        assert_eq!((held(tenant.id()), held(ArenaId::DEFAULT)), (Some(pages), Some(pages)));
    }

    /// TASK-234: what says an extent is reachable from a published generation says it of that
    /// extent alone: the cache moves no other's pages with it.
    #[tokio::test(flavor = "multi_thread")]
    #[should_panic(expected = "a tenant's copy moved for another extent")]
    async fn a_public_reach_moves_its_own_extent_alone() {
        let (store, _dir) = store(1 << 20).await;
        let (reached, other) = (id(&store, 1), id(&store, 2));
        write(&store, other, 0, &[1; BLOCK]).await.unwrap();
        store.commit(None).await.unwrap();
        let cached_in = CachedIn::Public(PublicReach(reached));
        let cached = cache::cached(&store.store, cached_in, None, None, other, BLOCK as u64);
        drop(cached.await);
    }

    /// doc-10: what a generation pins stays, whatever its store does next.
    #[tokio::test(flavor = "multi_thread")]
    async fn bases_outlast_what_their_stores_free() {
        let (store, dir) = store(1 << 20).await;
        let (publisher_path, base) = (dir.path().join("publisher"), dir.path().join("base"));
        let publisher = made(&publisher_path, 1 << 20).await;
        let extent = id(&publisher, 1);
        write(&publisher, extent, 0, &pattern(5, BLOCK)).await.unwrap();
        publisher.commit(None).await.unwrap();
        publisher.fork_to(&base, crate::host_cell::fork_child(publisher.cell())).await.unwrap();
        publisher.free(extent).unwrap();
        publisher.commit(None).await.unwrap();
        assert_eq!(read(&publisher, extent, 0, BLOCK).await, Err(Errno::ENOENT));

        let ancestry = OpenCell::open(&base).await.unwrap().ancestry();
        store.attach_base(&base, &ancestry).unwrap();
        assert_eq!(read(&store, extent, 0, BLOCK).await, Ok(pattern(5, BLOCK)));
    }

    /// doc-10: what is attached stays attached to forks, which a restored VM's store is, and through
    /// reopening, and doesn't reach back to the store a fork came from.
    #[tokio::test(flavor = "multi_thread")]
    async fn bases_stay_attached_through_forks_and_reopening() {
        let (store, dir) = store(1 << 20).await;
        let path = dir.path().join("store");
        let (base, later) = (dir.path().join("base"), dir.path().join("later"));
        let (extent, ancestry) = generation(&base, &pattern(6, BLOCK)).await;
        let (only_the_fork, later_ancestry) = generation(&later, &pattern(7, BLOCK)).await;
        let open = one_instance_each();
        store.open_bases(open.clone()).unwrap();
        store.attach_base(&base, &ancestry).unwrap();
        store.commit(None).await.unwrap();

        let fork = dir.path().join("fork");
        store.fork_to(&fork, crate::host_cell::fork_child(store.cell())).await.unwrap();
        let forked = OpenCell::open(&fork).await.unwrap();
        forked.open_bases(open.clone()).unwrap();
        assert_eq!(read(&forked, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));

        forked.attach_base(&later, &later_ancestry).unwrap();
        assert_eq!(read(&forked, only_the_fork, 0, BLOCK).await, Ok(pattern(7, BLOCK)));
        assert_eq!(read(&store, only_the_fork, 0, BLOCK).await, Err(Errno::ENOENT));

        drop(store);
        let store = OpenCell::open(&path).await.unwrap();
        assert_eq!(read(&store, extent, 0, BLOCK).await, Err(Errno::ENOENT), "not routed yet");
        store.open_bases(open).unwrap();
        assert_eq!(read(&store, extent, 0, BLOCK).await, Ok(pattern(6, BLOCK)));
    }

    /// doc-10: hundreds attached cost nothing until read: what reads one opens that one alone, and
    /// finding it takes a lookup of its cell.
    #[tokio::test(flavor = "multi_thread")]
    async fn bases_attached_open_when_read() {
        const ATTACHED: usize = 300;
        let (store, dir) = store(1 << 20).await;
        let mut bases: Vec<(ExtentId, Vec<u64>, std::path::PathBuf)> = Vec::new();
        for at in 0..ATTACHED {
            let path = dir.path().join(format!("base-{at}"));
            let (extent, ancestry) = generation(&path, &pattern(at as u8, BLOCK)).await;
            store.attach_base(&path, &ancestry).unwrap();
            bases.push((extent, ancestry, path));
        }
        assert_eq!(store.bases_opened(), 0);
        assert_eq!(store.bases.by_cell.read().unwrap().len(), 2 * ATTACHED);

        let (extent, ..) = &bases[ATTACHED / 2];
        assert_eq!(read(&store, *extent, 0, BLOCK).await, Ok(pattern((ATTACHED / 2) as u8, BLOCK)));
        assert_eq!(store.bases_opened(), 1);
        drop(bases);
    }

    /// doc-10: a follower reads what its writer has attached, which it pins as nothing, as nothing
    /// takes it away.
    #[tokio::test(flavor = "multi_thread")]
    async fn followers_read_through_to_what_their_writers_have_attached() {
        let (store, dir) = store(1 << 20).await;
        let base = dir.path().join("base");
        let (extent, ancestry) = generation(&base, &pattern(8, BLOCK)).await;
        let store = Arc::new(store);
        store.commit(None).await.unwrap();
        let follower = store.follow();
        store.attach_base(&base, &ancestry).unwrap();
        assert_eq!(read(&follower, extent, 0, BLOCK).await, Ok(pattern(8, BLOCK)));
        assert_eq!(follower.pin(extent), Ok(()));
        assert_eq!(follower.unpin(extent), Ok(()));
    }

    /// doc-10: a store's ancestors are never routed: what it inherited it holds, or freed.
    #[tokio::test(flavor = "multi_thread")]
    async fn bases_are_not_routed_for_what_a_store_inherited() {
        let (store, dir) = store(1 << 20).await;
        let inherited = id(&store, 1);
        write(&store, inherited, 0, &pattern(9, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        let (fork, base) = (dir.path().join("fork"), dir.path().join("base"));
        store.fork_to(&fork, crate::host_cell::fork_child(store.cell())).await.unwrap();
        let base_cell = crate::host_cell::fork_child(store.cell());
        store.fork_to(&base, base_cell).await.unwrap();
        let forked = OpenCell::open(&fork).await.unwrap();
        forked.attach_base(&base, &OpenCell::open(&base).await.unwrap().ancestry()).unwrap();
        assert!(!forked.bases.routes(inherited.cell));
        assert!(forked.bases.routes(base_cell.id));
        forked.free(inherited).unwrap();
        forked.commit(None).await.unwrap();
        assert_eq!(read(&forked, inherited, 0, BLOCK).await, Err(Errno::ENOENT));
    }

    /// TASK-160.10: a store whose directory can't be listed for its public markers, as with as
    /// many files open as the process may (`EMFILE`), fails to open, whichever way, with `EIO`:
    /// read as none, a public base opened private, its writer's, which a VM process opened to
    /// write.
    #[tokio::test(flavor = "multi_thread")]
    async fn stores_whose_public_markers_go_unread_fail_to_open() {
        let (base, dir) = store(1 << 20).await;
        base.make_public().unwrap();
        drop(base);
        let path = dir.path().join("store");
        fail::cfg("extents::public_cells", "return").unwrap();
        let opened = [
            OpenCell::open(&path).await.map(|store| store.is_public()),
            OpenCell::open_base(&path, None).await.map(|store| store.is_public()),
            OpenCell::open_without_profile(&path).await.map(|store| store.is_public()),
        ];
        fail::remove("extents::public_cells");
        for opened in opened {
            assert_eq!(opened.map_err(|err| err.errno()), Err(Some(Errno::EIO)));
        }
        assert!(OpenCell::open(&path).await.unwrap().is_public());
    }

    /// cladefs TASK-164.1 (backlog doc-13): a window that copies on write maps sealed extents and
    /// zeros alone, read-only: an extent written since the last commit it refuses to map (`EPERM`),
    /// the open extent's bytes left in the store. Sealed, a
    /// store to its page copies the page, the extent as the store's reads have it, and a commit
    /// leaves the private page. A follower takes no such window.
    #[tokio::test(flavor = "multi_thread")]
    async fn copying_windows_map_sealed_extents_alone() {
        let (store, _dir) = store(1 << 24).await;
        let ps = page_size();
        let window = store.create_copying_dax_window(4 * ps, None, 2).unwrap();
        let (sealed, open, never) = (id(&store, 1), id(&store, 2), id(&store, 3));
        write(&store, sealed, 0, &pattern(1, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        write(&store, open, 0, &pattern(2, BLOCK)).await.unwrap();
        assert_eq!(store.map(open, 0, ps, 0).await, Err(Errno::EPERM));
        assert_eq!(open_maps_refused(), 1, "an open extent's MAP uncounted");
        assert_eq!(store.map(never, 0, ps, 0).await, Err(Errno::ENOENT));
        assert_eq!(store.store.extent(open).map_err(errno), Ok(Some((BLOCK as u64, true))));
        assert_eq!(read(&store, open, 0, BLOCK).await, Ok(pattern(2, BLOCK)));

        store.map(sealed, 0, ps, 0).await.unwrap();
        block_on(window.fault(0, Access::Write)).unwrap();
        // SAFETY: The page is mapped, private, and nothing else accesses it.
        unsafe { *window.host_base() = 0xee };
        assert_eq!(store.private_pages(), Ok(1));
        assert_eq!(read(&store, sealed, 0, BLOCK).await, Ok(pattern(1, BLOCK)));
        store.commit(None).await.unwrap();
        assert_eq!(store.private_pages(), Ok(1));
        // SAFETY: As above.
        assert_eq!(unsafe { *window.host_base() }, 0xee);

        let again = store.create_copying_dax_window(ps, None, 1);
        assert_eq!(again.err().and_then(|error| error.errno()), Some(Errno::EEXIST));
        let follower = Arc::new(store).follow();
        let following = follower.create_copying_dax_window(ps, None, 1);
        assert_eq!(following.err().and_then(|error| error.errno()), Some(Errno::EROFS));
    }

    /// TASK-203: a store whose window holds a private page makes no version nor quiescent fork of
    /// itself, and lists no mappings of its window, which would name the page's source, not its
    /// bytes, as a snapshot of the VM would record them: `EBUSY`, saying how many. Rebound to an
    /// extent of its bytes, as the guest's freeze does, both are made (a VM restored reads them:
    /// cladefsd's snapshots_after_private_pages_are_rebound_restore_their_bytes).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_window_holding_private_pages_takes_no_snapshot() {
        let (store, dir) = store(1 << 24).await;
        let ps = page_size();
        let window = store.create_copying_dax_window(4 * ps, None, 2).unwrap();
        let (sealed, rebound) = (id(&store, 1), id(&store, 2));
        write(&store, sealed, 0, &pattern(1, BLOCK)).await.unwrap();
        store.commit(None).await.unwrap();
        store.map(sealed, 0, ps, 0).await.unwrap();
        block_on(window.fault(0, Access::Write)).unwrap();
        // SAFETY: The page is mapped, private, and nothing else accesses it.
        unsafe { *window.host_base() = 0xee };
        assert_eq!(store.private_pages(), Ok(1));
        let refused = store.make_version(None).await.unwrap_err();
        assert_eq!(refused.errno(), Some(Errno::EBUSY));
        assert!(format!("{refused:#}").contains("holds 1 private pages"), "{refused:#}");
        let forked = store
            .fork_quiescent_to(dir.path().join("fork"), crate::host_cell::fork_child(store.cell()))
            .await;
        assert_eq!(forked.err().and_then(|error| error.errno()), Some(Errno::EBUSY));
        assert_eq!(store.window_mappings(), Err(Errno::EBUSY));

        // The freeze's rebind: the page's bytes written to an extent, sealed, mapped over it.
        let mut bytes = pattern(1, ps as usize);
        bytes[0] = 0xee;
        write(&store, rebound, 0, &bytes).await.unwrap();
        store.commit(None).await.unwrap();
        store.map(rebound, 0, ps, 0).await.unwrap();
        assert_eq!(store.private_pages(), Ok(0));
        let contents = MappingContents::Extent { extent: rebound, offset: 0 };
        assert_eq!(
            store.window_mappings(),
            Ok(vec![WindowMapping { window_offset: 0, len: ps, contents }])
        );
        store.make_version(None).await.unwrap();
    }
}
