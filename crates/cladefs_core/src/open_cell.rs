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
//! open cell and window in the process shares: each a table of its pages, which are read in from
//! the open cell as they are first read or mapped, and evicted as memory runs out. Windows map them
//! read-only: one copy of each page, whichever open cell or fork maps it, as an extent never
//! changes once sealed, its ID its cell's to write once, which the open cell keeps to (TASK-79.6).
//! Its pages are read from any of the open cells that asked for it, open still, that hold it
//! (TASK-79.7). Reads of sealed extents read it too (TASK-32): the guest's filesystem reads its
//! metadata, which it maps not, so a fork reads what its base's other forks read before from
//! memory, not the open cell. Reads of open extents go to the open cell. The
//! open cell's files bypass the host's page cache.
//!
//! cladefs: windows map sealed extents and zeros alone, read-only (backlog doc-13): an open extent
//! is written only by `WRITE`, and a `MAP` of one fails (`EPERM`). A writer's window copies on
//! write: a write to a page gives the window a private copy of it (`crate::dax`), which no extent
//! names, so that no commit changes anything in a window.

mod opening;
#[cfg(test)]
mod tests;

pub use opening::Opening;

use crate::cache::{CachedExtent, CachedIn};
use crate::cell_dir;
use crate::dax::{
    Dax, DaxMapping, DaxWindow, SecondStage, Source, WindowChangeKind, WindowChanges,
};
use crate::error::{Errno, Error, Result};
use crate::faults::failpoint;
use crate::halt::{CellMutex, CellRwLock, Halt};
use crate::host_cell;
#[cfg(not(madsim))]
use crate::host_cell::CellOf;
use crate::memory::{VolatileSlice, VolatileSliceMut, gather, scatter};
use crate::pages::{Arena, ArenaId};
use crate::stats::{self, Counter};
use bucket_cell::cell::Kind;
use bucket_cell::layout::CellId;
use bucket_cell::trace;
use local_cell::{
    HostDir, LocalCell, NoSpace, NotQuiescent, SpentExtent, TooLong, TooSmall, Unaligned,
    Unsectored,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, PoisonError};
use storage_device::{ExtentId, NoExtent, OtherCell, SealedExtent};

/// What an open cell holds, as doc-4's `INFO` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtentInfo {
    /// The most bytes the open cell's extents may hold.
    pub capacity: u64,
    /// The bytes its extents hold.
    pub used: u64,
    /// The bytes it can still take.
    pub available: u64,
    /// One more than the highest sequence number of the extents the cell holds, as of
    /// its opening, and of those written since: where fxfs looks for orphans below. A number
    /// freed past it is minted no more, its writer minting from [`Info::sequence_floor`]
    /// (TASK-165.9).
    pub next_sequence: u64,
    /// cladefs: the lowest sequence number the guest may mint from: its writer's range's start,
    /// with an uploader (doc-7, The head), past which the open cell need hold nothing.
    pub sequence_floor: u64,
}

/// cladefs: what a cell holds, and whose it is: the bytes its extents hold (TASK-49), what
/// billing an open cell alone charges it and what it shares with the cells it was forked from; and
/// how many extents they are (TASK-454), which what an open cell holds in memory grows with
/// (TASK-443.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellHolding {
    /// The bytes its extents hold.
    pub bytes: Whose,
    /// How many extents it holds.
    pub extents: Whose,
}

/// cladefs: a count of what an open cell holds ([`CellHolding`]), split by whose it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Whose {
    /// Of the extents its cell wrote.
    pub own: u64,
    /// Of the extents it inherited from the cells it was forked from, which may still share
    /// them.
    pub inherited: u64,
}

/// A mapping of an open cell's DAX window, as a snapshot of its VM records it, to map it again
/// where the VM is restored ([`OpenCell::window_mappings`]).
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

/// cladefs: what an open cell opened is: who writes it, and whether every tenant reads it. It says
/// what may be done to it, which each of its methods that changes it checks, rather than each
/// caller (TASK-160.10). Fixed once it is open, with its uploader, which says whether it is a
/// version, as the extent daemon has it before anything asks it to do anything (`cladefsd::cells`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellAccess {
    /// Who writes it.
    pub role: CellRole,
    /// Whether its cell is public ([`OpenCell::make_public`]): a base every tenant's VM
    /// processes fork, its pages the default arena's, which each holds. As its directory said as
    /// it opened, which fails if it can't be read: an open cell made public is so to those opened
    /// since.
    pub public: bool,
}

/// cladefs: who writes an open cell ([`CellAccess`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellRole {
    /// Its writer: a guest's filesystem, or the work on it of whatever opened it.
    Writer,
    /// None: it is a version (doc-7, Versions), the name of a state, which any number of hosts read
    /// and fork at once.
    Version,
}

impl CellAccess {
    /// `Ok` if what the open cell holds may change: its extents and commits, which only its
    /// writer's do, and of no public base. A version refuses with `EROFS`; a
    /// public base with `EPERM`, as what changed would be every tenant's that forks it (TASK-43).
    pub fn may_write(self) -> Result<()> {
        match self {
            Self { public: true, .. } => Err(Errno::EPERM),
            Self { role: CellRole::Version, .. } => Err(Errno::EROFS),
            Self { role: CellRole::Writer, public: false } => Ok(()),
        }
    }

    /// `Ok` if the open cell's capacity may change, its writer's alone: `EROFS` for a version,
    /// whose forks take it as it is. A public base's grows, as no fork it made
    /// shares it.
    pub fn may_resize(self) -> Result<()> {
        match self.role {
            CellRole::Writer => Ok(()),
            CellRole::Version => Err(Errno::EROFS),
        }
    }

    /// `Ok` if the open cell may be forked or made public, which changes none of its extents, as a
    /// version and a public base may.
    pub fn may_manage(self) -> Result<()> {
        match self.role {
            CellRole::Writer | CellRole::Version => Ok(()),
        }
    }
}

/// cladefs: what a request changes of an open cell, which [`OpenCell::admit`] admits it to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Changing {
    /// What it holds: its extents and commits ([`CellAccess::may_write`]).
    Contents,
    /// Its capacity ([`CellAccess::may_resize`]).
    Capacity,
    /// Its cell, not what it holds: its versions, forks and publicity
    /// ([`CellAccess::may_manage`]).
    Cell,
}

/// cladefs: witness that an open cell's standing was checked by [`OpenCell::admit`] before a
/// mutating operation runs (TASK-636, board 2702).
#[must_use]
pub(super) struct Admitted<'a>(&'a OpenCell);

impl Admitted<'_> {
    fn local(&self) -> &Arc<LocalCell<HostDir>> {
        &self.0.local
    }

    fn uploader(&self) -> Option<&local_cell::upload::Uploader<HostDir>> {
        self.0.uploader.as_ref()
    }
}

/// An open cell: the extent store a guest's filesystem lives on. See the module docs.
pub struct OpenCell {
    /// cladefs: the host context this open cell runs on (board 10962).
    pub host: Arc<crate::Host>,
    local: Arc<LocalCell<HostDir>>,
    /// cladefs: the bucket the open cell's description names, which its cell is kept in
    /// (`host_cell::Opened`, TASK-165.6).
    bucket: Arc<bucket_cell::object_bucket::RoutedBucket>,
    /// cladefs: where it was opened: its directory, which keeps what it was forked from.
    path: std::path::PathBuf,
    dax: CellMutex<Option<Dax>>,
    /// cladefs: the arenas of physical memory its extents' pages are of (TASK-43).
    arenas: Arenas,
    /// cladefs: the open cells it was forked from, nearest first, as of the forks
    /// ([`cell_dir::ancestry`]).
    forked_from: Arc<Vec<(u64, std::path::PathBuf)>>,
    /// cladefs: the uploader of the cell to object storage, which opened it, taking its
    /// head, or holds its version: what doc-4's `COMMIT_UPLOADED` commits wait for (TASK-165.9);
    /// none once its close took it.
    uploader: Option<local_cell::upload::Uploader<HostDir>>,
    /// cladefs: of an ephemeral cell's open cell, its keys held among the host's while it is open,
    /// as an uploader holds a bucket cell's (doc-25, TASK-698.7).
    keys: Option<bucket_cell::keys::RingHold>,
    /// cladefs: of an open cell opened for a guest, a fork of a version, its version's read
    /// evidence of its tenant's: what predicts its reads, fetched ahead, and its guest's boot,
    /// measured against it, and recorded, if it records (`crate::evidence`, TASK-702.2,
    /// TASK-702.3).
    evidence: Option<crate::evidence::Evidence>,
    /// cladefs: held, shared, by writes, which open extents, and alone by a fork only once nothing
    /// is open, which none comes between ([`OpenCell::fork_quiescent`]).
    opening: CellRwLock<()>,
    /// cladefs: the window's latest changes, which a refused fault's log tells of the page it was
    /// on ([`OpenCell::refused`], TASK-44.12).
    changes: CellMutex<WindowChanges>,
    /// cladefs: the turns its daemon has taken at making its commits local behind them
    /// ([`OpenCell::flush_turned`]): how a commit's lag is told in turns, which the daemon's
    /// load stretches, rather than in time (TASK-110).
    flush_turns: std::sync::atomic::AtomicU64,
    /// cladefs: what each of its locks is locked through: set by a panic holding one, after which
    /// it serves nothing more (`crate::halt`, TASK-153.16).
    halt: Halt,
    /// cladefs: whether it was closed ([`OpenCell::close`]), which its drop then does again not.
    closed: bool,
    /// cladefs: the incarnation of the host it was opened on: one gone since, killed, closes
    /// nothing, as a killed process doesn't (board 14567).
    incarnation: node_local::Incarnation,
    /// cladefs: what reads its extents whose data is in the bucket: of the bucket its uploader
    /// keeps the cell in, keeping what it fetches in the host's cache, if it has one.
    fetcher: Arc<crate::bucket::HostFetcher>,
    /// cladefs: invoked once after all other fields of this open cell have dropped (board 8237,
    /// board 11701): last field so its `Drop` runs after `local` and `uploader` are closed.
    on_close: OnClose,
}

/// Typed close hook stored as the final field of [`OpenCell`] so it fires after the cell's
/// underlying storage and uploader have fully dropped (board 8237, board 11701).
#[derive(Default)]
struct OnClose(std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>);

impl Drop for OnClose {
    fn drop(&mut self) {
        if let Some(callback) = self.0.get_mut().unwrap_or_else(PoisonError::into_inner).take() {
            callback();
        }
    }
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

/// cladefs: what a VM snapshot records for one extent device (`CellClient::snapshot_cell`):
/// either a bucket-backed version (restored by forking on any host), or a host-local ephemeral
/// cell quiesced at its latest commit (restored in place on the same host).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CellSnapshot {
    /// A bucket-backed version of a cell, forked on restore.
    Version(CellVersion),
    /// A host-local ephemeral cell quiesced at commit `committed`, reopened in place on restore
    /// (`EEXIST` if the cell was modified or replaced since the snapshot).
    LocalEphemeral {
        /// The cell's tenant.
        tenant: u64,
        /// The cell's ID.
        cell: u64,
        /// The cell's latest commit number when the snapshot was taken.
        committed: u64,
    },
}

/// cladefs: a version of a cell ([`OpenCell::versions`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionInfo {
    /// Its ID, unique in its cell.
    pub id: u64,
    /// Whether the cell's head names it: forks of it on other hosts are made then.
    pub uploaded: bool,
    /// Whether forks of it into other tenants' cells are made.
    pub shared: bool,
}

/// cladefs: the `MAP`s of open extents refused (backlog doc-13), which no guest makes: a tripwire,
/// which the daemon's metrics show.
// The process's count, every simulated host's together, as a process's metrics are.
static OPEN_MAPS_REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// cladefs: the `MAP`s of open extents refused so far.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn open_maps_refused() -> u64 {
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

/// What a commit asks to wait for (board 7409): a guest's commit, which a replicated cell upgrades
/// to its `replicated.fsync` / `Replicated` level (doc-21), or an ordering-only pre-barrier
/// (`WRITE_BARRIER`), which orders prior writes ahead of the write it precedes without waiting for
/// durability.
#[derive(Clone, Copy)]
enum CommitRequest {
    Commit(Commit),
    Barrier,
}

impl CommitRequest {
    fn level(self) -> &'static str {
        match self {
            Self::Commit(commit) => commit.level(),
            Self::Barrier => Commit::Ordered.level(),
        }
    }
}

/// When a commit completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commit {
    /// Once ordered: the open cell makes it local soon after.
    Ordered,
    /// cladefs: once local, synced to the host's disk, not waiting for the bucket (`COMMIT_LOCAL`):
    /// what a database's commit waits for, as SQLite's own does on a local filesystem (TASK-301).
    Local,
    /// Once uploaded (`COMMIT_UPLOADED`): its segments acknowledged by the bucket, if the open cell
    /// has one; local, if not.
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
/// open cell failed to read or write.
fn upload_errno(failure: local_cell::upload::Failure) -> Errno {
    crate::bucket::errno(&failure).unwrap_or_else(|| {
        log::error!("cladefs: uploading the open cell's commits: {failure}");
        Errno::EIO
    })
}

/// cladefs: the errno of a read from the bucket that failed with `error`: the uploader's failure's,
/// or the open cell's (TASK-165.8).
fn fetch_errno(error: anyhow::Error) -> Errno {
    match error.downcast::<local_cell::upload::Failure>() {
        Ok(failure) => upload_errno(failure),
        Err(error) => errno(error),
    }
}

/// cladefs: lets `cell` go, and, if this was its last reference, closes it ([`OpenCell::close`]),
/// holding no thread as it waits: as every holder of the daemon's open cells lets one go (doc-24,
/// step 9), so that whichever holds it last, a connection's teardown, a request or a fork, closes it,
/// awaited, and none closes it in its drop, which blocks.
pub async fn release(cell: Arc<OpenCell>) {
    if let Some(cell) = Arc::into_inner(cell) {
        // Mark closing synchronously before awaiting close so a concurrent opener waits for close
        // instead of observing `claimed[id] == false` and failing with `EBUSY` (board 11340).
        cell.mark_closing();
        cell.close().await;
    }
}

/// cladefs: the span of a commit's wait for object storage to hold the commits up to `epoch`, or
/// as many of them as its lag allows (`bucket_cell::trace`, TASK-258).
fn upload_wait(epoch: u64) -> trace::Span {
    let span = trace::Span::start("commit.upload_wait");
    span.set_count(trace::COMMIT, epoch);
    span
}

/// The errno of the open cell's error, as doc-4 has them: each of the open cell's errors its own,
/// and any other the open cell failing to read or write, `EIO`, logged. The one list of them, which
/// every request shares.
pub fn errno(error: anyhow::Error) -> Errno {
    if let Some(&errno) = error.downcast_ref::<Errno>() {
        // cladefs: one said already, as a halt's (`crate::halt`), passed on as it was.
        errno
    } else if error.is::<NoExtent>() {
        Errno::ENOENT
    } else if error.is::<SealedExtent>() || error.is::<SpentExtent>() || error.is::<OtherCell>() {
        Errno::EPERM
    } else if error.is::<NoSpace>()
        || error.is::<TooSmall>()
        || error.is::<local_cell::footprint::DiskFull>()
    {
        // cladefs: and its disk, or its tenant's limits, refusing it (doc-22).
        Errno::ENOSPC
    } else if error.is::<Unaligned>()
        || error.is::<Unsectored>()
        || error.is::<TooLong>()
        || error.is::<local_cell::LastSequence>()
    {
        Errno::EINVAL
    } else if full_disk(&error) {
        // cladefs: the host's disk refusing what the open cell makes for the request, a raw file or
        // a segment's, its admission past (doc-22, TASK-539.12): a full disk to the guest, as its
        // admission's refusal is, not an I/O error. An open cell failed says so instead, and has
        // none.
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

// cladefs: closing the open cell frees its cell to be opened again, though caches of its extents
// may hold its open cell still, to read them.
impl Drop for OpenCell {
    fn drop(&mut self) {
        // cladefs: closed already, as the daemon closes its open cells (TASK-685.3).
        if self.closed {
            return;
        }
        // cladefs: the daemon closes every open cell it has, awaited ([`release`]): under madsim,
        // an open cell dropped open is one it let go of where it couldn't wait (doc-24, step 9);
        // but for its host's dying, killed, which closes nothing, as a process killed doesn't.
        // cladefs: nor one of an incarnation killed since, whose tasks the simulation may drop
        // only as the host started again (`node_local::Incarnation`, board 14567).
        if cfg!(madsim) && self.incarnation.is_gone() {
            return;
        }
        if cfg!(madsim) && !std::thread::panicking() {
            panic!(
                "an open cell dropped open, {}: the daemon closes its open cells \
                 (open_cell::release)",
                self.path.display()
            );
        }
        // cladefs: its guest's boot, measured and written behind it as its evidence goes.
        drop(self.evidence.take());
        // cladefs: its uploader first, which closes its open segment, uploads what it made local
        // and stops (TASK-192); then the open cell. Closing from the first: what waits for the
        // close, as a fork of a version it made does, waits for its uploads too (TASK-644).
        self.local.closing();
        fail::fail_point!("open_cell::closing");
        drop(self.uploader.take());
        // cladefs: dropped unclosed, outside the simulation, closed here, waiting on the dropping
        // thread, as its uploader's drop waits (board 2944): what closes an open cell awaits its
        // close ([`release`]).
        if let Err(err) = local_cell::wait_here(self.local.close()) {
            log::error!("cladefs: closing an extent store: {err:#}");
        }
    }
}

impl OpenCell {
    /// cladefs: marks this open cell's directory claim as closing synchronously before spawning or
    /// awaiting [`OpenCell::close`], closing the window where a concurrent opener (`Here::open`,
    /// `sync`, `opened_version`) could observe `claimed[id] == false` and fail with `EBUSY`
    /// (board 11340).
    pub fn mark_closing(&self) {
        self.local.closing();
    }

    /// cladefs: closes it as dropping it does, and returns once it is closed: its uploader's
    /// uploads done and stopped (`Uploader::close`), and the open cell closed, which may be opened
    /// again at once (TASK-685.3). Holds no thread while it waits for its uploader, as the daemon
    /// closes its open cells; dropped unclosed, it blocks.
    pub async fn close(mut self) {
        // Mark closing synchronously before any `.await` so concurrent openers wait (board 11340).
        self.local.closing();
        // cladefs: its guest's boot, measured and written first.
        if let Some(evidence) = self.evidence.take() {
            evidence.close().await;
        }
        fail::fail_point!("open_cell::closing");
        if let Some(uploader) = self.uploader.take() {
            uploader.close().await;
        }
        if let Err(err) = self.local.close().await {
            log::error!("cladefs: closing an extent store: {err:#}");
        }
        self.closed = true;
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

    /// A new cell directory at `path`, which must not exist, holding at most `capacity` bytes, with
    /// a cell of its own. cladefs: kept in this host's bucket, or, where this is no host of
    /// cladefs's system, a file bucket of its own, which its description names
    /// (`host_cell::create`, TASK-165.6), opened as this process opens open cells
    /// ([`Opening::here`]).
    #[cfg(not(madsim))]
    pub async fn create(path: impl AsRef<Path>, capacity: u64) -> Result<Self, Error> {
        Opening::here()?.create(path, capacity, CellOf::New(host_cell::DEFAULT_TENANT)).await
    }

    /// cladefs: runs `callback` once the open cell finishes dropping, after `local` and `uploader`
    /// have closed: notifies the daemon's cell table that a departing writer's cell is closed
    /// (`crate::cell_table::CellTable::closed`, board 8237, board 11701).
    // The extent daemon's (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn on_close(&self, callback: impl FnOnce() + Send + 'static) {
        let prev = self
            .on_close
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Box::new(callback));
        debug_assert!(prev.is_none(), "OpenCell::on_close registered twice");
    }

    /// cladefs: what it is, which says what may be done to it ([`CellAccess`]).
    pub fn access(&self) -> CellAccess {
        let at_version =
            self.uploader.as_ref().is_some_and(local_cell::upload::Uploader::is_at_version);
        let role = if at_version { CellRole::Version } else { CellRole::Writer };
        CellAccess { role, public: self.is_public() }
    }

    /// cladefs: halts the open cell, as `why` says, unless it is already: a panic went through it
    /// past its locks, as one serving a request of it, which a panic holding one of them does
    /// itself. It serves nothing more ([`crate::halt`], TASK-153.16).
    // The extent daemon's (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn halt(&self, why: impl FnOnce() -> String) {
        self.halt.halt(why);
    }

    /// cladefs: whether the open cell is halted ([`OpenCell::halt`]): its requests fail with `EIO`,
    /// or `EACCES` once its host forgot its tenant's or cell's keys (`crate::bucket::forget`).
    // The extent daemon's (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn is_halted(&self) -> bool {
        self.halt.is_halted()
    }

    /// cladefs: the errno the open cell's requests fail with, if it is halted
    /// ([`OpenCell::is_halted`]).
    // The extent daemon's (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn halted(&self) -> Option<Errno> {
        self.halt.errno()
    }

    /// cladefs: what has its uploader, once it stops, stop at once, uploading nothing more, which
    /// its write-ahead log keeps for its next opening (TASK-216): what cladefsd has a writer that
    /// waits for its going connection's open cell to close call. None for an open cell with no
    /// uploader.
    pub fn cut_short(&self) -> Option<crate::cell_table::Wanted> {
        let uploader = self.uploader.as_ref()?;
        Some(std::sync::Arc::new(uploader.cut_short()))
    }

    /// The number of the open cell's latest commit.
    pub fn committed(&self) -> Result<u64> {
        self.local.last_commit().map_err(errno)
    }

    /// cladefs: whether this open cell is a host-local ephemeral cell (doc-25).
    pub fn is_ephemeral(&self) -> bool {
        self.local.is_ephemeral()
    }

    /// cladefs: the kind of cell this open cell serves (`Fxfs`, `Sqlite`, or `Memory`).
    pub fn kind(&self) -> Option<Kind> {
        self.local.kind()
    }

    /// cladefs: quiesces an ephemeral cell for a host-local VM snapshot: verifies no extent is
    /// open (`EBUSY` otherwise), flushes every ordered commit durably to the host's disk, folds
    /// closed segments into its local checkpoint (punching freed runs), and returns the latest
    /// commit number to guard stale restores (`EEXIST`). `EINVAL` for a bucket-backed cell.
    pub async fn quiesce_ephemeral(&self) -> Result<u64, Error> {
        let admitted = self.admit(Changing::Cell)?;
        if !self.is_ephemeral() {
            return Err(Errno::EINVAL.into());
        }
        self.at_rest(&admitted, async {
            let committed = self.make_local().await?;
            self.local.checkpoint(true).await.map_err(Error::new)?;
            Ok(committed)
        })
        .await
    }

    /// cladefs: what reads the extents the open cell holds whose data is in the bucket, at their
    /// locations there, keeping what it fetches in the host's cache, if its uploader has one
    /// (TASK-165.8).
    fn fetcher(&self) -> Arc<crate::bucket::HostFetcher> {
        self.fetcher.clone()
    }

    /// cladefs: forks the open cell, as of its last commit, made uploaded first, and returns the
    /// fork's cell: a new cell of `cell`, its ID the bucket's to give (`bucket_cell::objectid`,
    /// TASK-364), holding what it holds, whose open cell is a new directory at `path`, which
    /// [`OpenCell::open`] opens: `EEXIST` if anything is at `path`. The fork is made in the
    /// bucket alone, by the open cell's uploader, which keeps its cell in the bucket its
    /// description names: a commit naming the child, whose acknowledgement gives the child its
    /// record, inheriting the open cell's extents rather than copies (doc-7, Forks). The child's
    /// directory is made then, its open cell recovering the child from the bucket as it opens, its
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

    /// cladefs: forks the open cell, at a version, to a new cell of `tenant`'s, an ephemeral cell
    /// kept on this host alone, whose open cell is made at `path` (doc-25, TASK-698.3;
    /// `host_cell::fork_ephemeral`), and returns its cell: `EINVAL` of an open cell at no version,
    /// `ENOTSUP` on a host keeping no ephemeral cells.
    pub async fn fork_ephemeral(
        &self,
        path: impl AsRef<Path>,
        tenant: u64,
    ) -> Result<CellId, Error> {
        let admitted = self.admit(Changing::Cell)?;
        let uploader = admitted.uploader().ok_or(Errno::EROFS)?;
        if !uploader.is_at_version() {
            return Err(Errno::EINVAL.into());
        }
        let cell = self.new_child(tenant).await?;
        let child = bucket_cell::layout::Child { cell, owner: rand::random() };
        let inherited = |making: &mut local_cell::Making<'_, HostDir>| -> anyhow::Result<()> {
            Ok(cell_dir::inherit(&self.path, self.cell().id, &self.forked_from, making)?)
        };
        let forking = host_cell::fork_ephemeral_on(
            &self.host,
            admitted.local(),
            uploader,
            path.as_ref(),
            child,
            inherited,
        );
        self.fork_span(cell).run_async(forking).await?;
        Ok(cell)
    }

    /// cladefs: forks the open cell, at a version, to a new ephemeral cell of `tenant`'s placed
    /// directly inside `parent_dir/<CELL_PADDED>` so the ephemeral host journal records its final
    /// canonical path without a post-create rename (hotbed question 547).
    pub async fn fork_ephemeral_in(
        &self,
        parent_dir: impl AsRef<Path>,
        tenant: u64,
    ) -> Result<(CellId, std::path::PathBuf), Error> {
        let admitted = self.admit(Changing::Cell)?;
        let uploader = admitted.uploader().ok_or(Errno::EROFS)?;
        if !uploader.is_at_version() {
            return Err(Errno::EINVAL.into());
        }
        let cell = self.new_child(tenant).await?;
        let path = parent_dir.as_ref().join(base32_id::Padded(cell.id).to_string());
        let child = bucket_cell::layout::Child { cell, owner: rand::random() };
        let inherited = |making: &mut local_cell::Making<'_, HostDir>| -> anyhow::Result<()> {
            Ok(cell_dir::inherit(&self.path, self.cell().id, &self.forked_from, making)?)
        };
        let forking = host_cell::fork_ephemeral_on(
            &self.host,
            admitted.local(),
            uploader,
            &path,
            child,
            inherited,
        );
        self.fork_span(cell).run_async(forking).await?;
        Ok((cell, path))
    }

    /// cladefs: a fork's child, the cell of a new cell of `tenant`'s, its ID the open cell's
    /// bucket's to give. One with no uploader forks nothing (`EROFS`). An open cell with a writer
    /// forks to no
    /// cell of another tenant's than its own unless it is the default tenant's (`EACCES`,
    /// `host_cell::may_fork_into`, TASK-395.9): refused before the bucket gives an ID.
    async fn new_child(&self, tenant: u64) -> Result<CellId, Error> {
        let admitted = self.admit(Changing::Cell)?;
        let uploader = admitted.uploader().ok_or(Errno::EROFS)?;
        host_cell::may_fork_into(self.cell(), uploader, tenant)?;
        Ok(host_cell::new_cell(uploader.bucket(), tenant).await.map_err(upload_errno)?)
    }

    /// cladefs: forks the open cell as [`OpenCell::fork`] does, to `cell`, whatever its ID: so the
    /// tests make a fork whose child's ID another cell took, which IDs given out by the bucket
    /// never are, to show a fork refuses it (`EEXIST`) and harms no other cell (TASK-79.82).
    pub async fn fork_to(&self, path: impl AsRef<Path>, cell: CellId) -> Result<(), Error> {
        let forking = async {
            let admitted = self.admit(Changing::Cell)?;
            self.fork_at_version(&admitted, path.as_ref(), cell, false).await
        };
        self.fork_span(cell).run_async(forking).await
    }

    /// cladefs: forks the open cell into a pre-allocated `cell` after checking cross-tenant fork
    /// permission ([`host_cell::may_fork_into`]), so daemon restore forks can place directly at
    /// `<disk>/cells/<TENANT>/<CELL>` without losing deferred durability (board 7420).
    pub async fn fork_into(&self, path: impl AsRef<Path>, cell: CellId) -> Result<(), Error> {
        let forking = async {
            let admitted = self.admit(Changing::Cell)?;
            let uploader = admitted.uploader().ok_or(Errno::EROFS)?;
            host_cell::may_fork_into(self.cell(), uploader, cell.tenant)?;
            self.fork_at_version(&admitted, path.as_ref(), cell, false).await
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

    /// cladefs: forks the open cell, as [`OpenCell::fork`] says, to `cell` at `path`: an open cell
    /// at a version forks itself; any other, a version of its last commit, made (of it quiescent,
    /// if `quiescent`) and uploaded, an open cell at which, among the host cache's cells at
    /// versions ([`host_cell::version_dir`]), forks, then dropped, its child keeping what it
    /// inherits by its reference. Forks are of versions alone (TASK-508.3).
    async fn fork_at_version(
        &self,
        admitted: &Admitted<'_>,
        path: &Path,
        cell: CellId,
        quiescent: bool,
    ) -> Result<(), Error> {
        let uploader = admitted.uploader().ok_or(Errno::EROFS)?;
        // cladefs: the child's owner, which every record of it in object storage names, whoever
        // writes it, so that another fork to its ID, whose owner differs, is told apart
        // (TASK-79.82, TASK-153.9).
        let child = bucket_cell::layout::Child { cell, owner: rand::random() };
        // cladefs: what is attached to it stays attached to the fork, a restored VM's too; and what
        // is public of it, and what it was forked from.
        let inherited = |making: &mut local_cell::Making<'_, HostDir>| -> anyhow::Result<()> {
            Ok(cell_dir::inherit(&self.path, self.cell().id, &self.forked_from, making)?)
        };
        // Refused before anything is made, the version among it.
        host_cell::may_fork_into(self.cell(), uploader, cell.tenant)?;
        // cladefs: at rest, if `quiescent`, until its version is committed, no write coming
        // between: what follows forks the version, its writes after it its own (TASK-685.3).
        let id = match (uploader.is_at_version(), quiescent) {
            (true, quiescent) => {
                if quiescent {
                    self.at_rest(admitted, async { Ok(()) }).await?;
                }
                return host_cell::fork_on(
                    &self.host,
                    admitted.local(),
                    uploader,
                    path,
                    child,
                    inherited,
                )
                .await;
            }
            (false, true) => {
                self.at_rest(admitted, self.commit_version(admitted, None, true)).await?
            }
            (false, false) => self.commit_version(admitted, None, false).await?,
        };
        let uploaded = async {
            self.make_uploaded().await?;
            failpoint!("fork::uploaded");
            Ok::<(), Error>(())
        }
        .await;
        if let Err(err) = uploaded {
            // cladefs: undo the scratch version committed for this fork when uploading or the
            // handoff fails before `fork_through_on` takes ownership of it (board 5546, TASK-639).
            if let Err(drop_err) =
                uploader.drop_versions(&std::collections::BTreeSet::from([id])).await
            {
                log::error!(
                    "cladefs: fork of {:?} failed before fork_through, its version {id:x} not dropped: {drop_err}",
                    self.cell(),
                );
            }
            return Err(err);
        }
        host_cell::fork_through_on(
            &self.host,
            admitted.local(),
            uploader,
            id,
            path,
            child,
            inherited,
        )
        .await
    }

    /// cladefs: the bucket the open cell's description names, which its cell is kept in: what its
    /// daemon attaches it to (TASK-165.6).
    pub fn bucket(&self) -> &Arc<bucket_cell::object_bucket::RoutedBucket> {
        &self.bucket
    }

    /// cladefs: its open cell, for tests of what it holds.
    #[cfg(any(test, feature = "testing"))]
    pub fn local(&self) -> &Arc<LocalCell<HostDir>> {
        &self.local
    }

    /// cladefs: the open cell's uploader, if it has one.
    #[cfg(test)]
    pub fn uploader(&self) -> Option<&local_cell::upload::Uploader<HostDir>> {
        self.uploader.as_ref()
    }

    /// cladefs: whether its open cell's map of the extents the bucket holds of its cell is
    /// `other`'s, one map shared (`local_cell::LocalCell::shares_extents_with`, TASK-453).
    #[cfg(test)]
    pub fn shares_extents_with(&self, other: &Self) -> bool {
        self.local.shares_extents_with(&other.local).unwrap()
    }

    /// cladefs: whether what its guest reads now is its boot's, which its version's read evidence
    /// measures, and may record ([`OpenCell::guest_read`], TASK-702.2, TASK-702.3).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn records_reads(&self) -> bool {
        self.evidence.as_ref().is_some_and(crate::evidence::Evidence::records)
    }

    /// cladefs: says its guest read `len` bytes at `offset` in `extent`, begun at `began`, by a
    /// `READ`, a `MAP` or a fault, which its boot takes, if it is within it
    /// ([`OpenCell::records_reads`], TASK-702.2).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn guest_read(&self, extent: ExtentId, offset: u64, len: u64, began: std::time::Instant) {
        if let Some(evidence) = &self.evidence {
            evidence.read(extent, offset, len, began);
        }
    }

    /// cladefs: of a fork of a version, the version and its keys, as its uploader says them
    /// (`Uploader::forked_from`): for tests of its read evidence.
    #[cfg(any(test, feature = "testing"))]
    pub fn forked_from(&self) -> Option<local_cell::upload::Forked> {
        self.uploader.as_ref()?.forked_from()
    }

    /// cladefs: of an open cell opened for a guest, a fork of a version, what its version's read
    /// evidence predicts it reads (TASK-702.2).
    #[cfg(test)]
    pub fn predicted(&self) -> Option<crate::evidence::Predicted> {
        self.evidence.as_ref().map(crate::evidence::Evidence::predicted)
    }

    /// cladefs: of an open cell opened for a guest, a fork of a version, its version's read
    /// evidence: for tests of what it does.
    #[cfg(any(test, feature = "testing"))]
    pub fn evidence(&self) -> Option<&crate::evidence::Evidence> {
        self.evidence.as_ref()
    }

    /// cladefs: `extent`'s length, and whether it is open, if its open cell holds it, as reads ask
    /// (`local_cell::LocalCell::extent`); and as traces of reads name it (TASK-702.1).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn stored_extent(&self, extent: ExtentId) -> Result<Option<(u64, bool)>> {
        self.local.extent(extent).map_err(errno)
    }

    /// cladefs: a read's of `extent` from the extent cache ([`OpenCell::read_cached`]).
    #[cfg(test)]
    pub async fn read_from_cache(
        &self,
        extent: ExtentId,
        offset: u64,
        data: &mut [u8],
    ) -> Result<bool> {
        self.read_cached(extent, offset, data).await
    }

    /// The cell whose extents the guest mints, by its ID, in its cell.
    pub fn cell(&self) -> CellId {
        self.local.description().cell
    }

    /// cladefs: the bytes the cell's files take of the host's disk, as its open cell's footprint
    /// counts them: what is not acknowledged in the bucket yet, and the open segment (doc-22).
    pub fn disk_bytes(&self) -> u64 {
        self.local.dir().footprint().bytes()
    }

    /// What the open cell holds (`INFO`).
    pub fn info(&self) -> Result<ExtentInfo> {
        let info = self.local.info().map_err(errno)?;
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
        let info = self.local.info().map_err(errno)?;
        Ok(CellHolding {
            bytes: Whose { own: info.own, inherited: info.inherited },
            extents: Whose { own: info.own_extents, inherited: info.inherited_extents },
        })
    }

    /// Sets the most bytes the open cell's extents may hold, in a commit of its own: grows it, or
    /// shrinks it no lower than what its extents hold (`ENOSPC`). The guest's filesystem, which may
    /// hold more, agrees to a shrink first (the extent daemon's `RESIZE` asks it).
    pub async fn set_capacity(&self, capacity: u64) -> Result<()> {
        let admitted = self.admit(Changing::Capacity)?;
        admitted.local().set_capacity(capacity).await.map_err(errno)
    }

    /// Reads the bytes at `offset` in `extent` into `buffers`, in order (`READ`). Blocks never
    /// written read as zeros. cladefs: what it reads in, from the bucket or the cache's page-ins,
    /// awaited, holding no thread (doc-24, step 5).
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
        // cladefs: a piece at a time, as the extent daemon's memory holds no more of a request
        // (TASK-149.17); once at least, as a read of nothing finds whether the extent is there.
        let mut piece = vec![0; len.min(Self::PIECE)];
        let mut done = 0;
        loop {
            let data = &mut piece[..(len - done).min(Self::PIECE)];
            let at = offset.checked_add(done as u64).ok_or(Errno::EINVAL)?;
            // cladefs: re-check halt on each piece and before publishing plaintext into the
            // caller's buffers so a concurrent `forget` cannot slip data out (board 5542,
            // board 2704).
            self.halt.check()?;
            if !self.read_cached(extent, at, data).await? {
                self.halt.check()?;
                self.read_stored(extent, at, data).await?;
            }
            self.halt.check()?;
            scatter(buffers, done, data);
            done += data.len();
            if done == len {
                return Ok(());
            }
        }
    }

    /// cladefs: reads as [`OpenCell::read`] does, into `data`, a buffer of this process's own, from
    /// the cell itself, or object storage, never through the extent cache: for a device this
    /// process mounts a filesystem on (`storage::device`), whose kernel caches the pages it reads
    /// itself, which the extent cache would hold a second time, at 2.4 times the cost of a read
    /// (board 8260). The extent cache is for what windows map, and their guests read.
    pub async fn read_uncached(
        &self,
        extent: ExtentId,
        offset: u64,
        data: &mut [u8],
    ) -> Result<()> {
        stats::add(Counter::Reads, 1);
        stats::add(Counter::ReadBytes, data.len() as u64);
        self.halt.check()?;
        self.read_stored(extent, offset, data).await?;
        self.halt.check()
    }

    /// cladefs: reads `extent`'s bytes at `offset` from the extent cache, if it is sealed: which
    /// pages them in from the open cell, or object storage, unless they are in memory already, read
    /// by any open cell or fork before (TASK-32). Returns whether it read them: not an open
    /// extent's, nor one the open cell doesn't hold.
    async fn read_cached(&self, extent: ExtentId, offset: u64, data: &mut [u8]) -> Result<bool> {
        self.halt.check()?;
        let Some((len, false)) = self.local.extent(extent).map_err(errno)? else {
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
            Ok(()) => {
                self.halt.check()?;
                Ok(true)
            }
            // A revoked or halted cell must never fall back to `read_stored` (board 5542,
            // board 2704).
            Err(Errno::EACCES) => Err(Errno::EACCES),
            // Freed meanwhile, or its page-in failed: as the open cell says.
            Err(errno) => {
                self.halt.check()?;
                log::debug!("cladefs: reading {extent:?} at {offset} from the cache: {errno:?}");
                Ok(false)
            }
        }
    }

    /// Reads a sealed extent's bytes from the open cell, or, cladefs, from object storage, if the
    /// open cell holds it without its data.
    async fn read_stored(&self, extent: ExtentId, offset: u64, data: &mut [u8]) -> Result<()> {
        stats::add(Counter::ReadStoredBytes, data.len() as u64);
        match local_cell::upload::blocking(|| self.local.read(extent, offset, data)) {
            Err(ref error)
                if let Some(in_bucket) = error.downcast_ref::<local_cell::BucketData>() =>
            {
                let (location, len) = (in_bucket.location.clone(), data.len() as u64);
                // cladefs: what the cell holds of it now, if a relocation moved it meanwhile
                // (doc-27), read as first, into a buffer allocated only if the bucket reports it
                // lost (board 12313).
                let again = || {
                    let mut here = vec![0; data.len()];
                    local_cell::upload::blocking(|| self.local.read(extent, offset, &mut here))?;
                    Ok(here)
                };
                let fetcher = self.fetcher();
                let followed = fetcher.read_following(extent, location, (offset, len), again);
                match followed.await.map_err(fetch_errno)? {
                    crate::bucket::Followed::Fetched(fetched) => {
                        data[..fetched.len()].copy_from_slice(&fetched);
                        data[fetched.len()..].fill(0);
                    }
                    crate::bucket::Followed::Here(here) => data.copy_from_slice(&here),
                }
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
            self.commit_request(None, CommitRequest::Barrier).await?;
        }
        self.write(extent, offset, buffers).await?;
        match barriers.after {
            None => Ok(()),
            Some(commit) => self.commit_with(barriers.root, commit).await,
        }
    }

    /// Writes the bytes of `buffers`, in order, at `offset` in `extent`, which must be the open
    /// cell's cell's, and open (`WRITE`).
    pub async fn write(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
    ) -> Result<()> {
        let admitted = self.admit(Changing::Contents)?;
        let len = buffers.iter().map(VolatileSlice::len).sum::<usize>();
        let _opening = self.opening.read(&self.halt).await?;
        // cladefs: from `buffers` to the open cell's file, a piece at a time, as the extent
        // daemon's memory holds no more of a request (TASK-149.17).
        let fill = |at: u64, piece: &mut [u8]| {
            gather(buffers, at as usize, piece);
        };
        let written = admitted.local().write_with(extent, offset, len as u64, Self::PIECE, fill);
        written.await.map_err(errno)
    }

    /// Frees `extent`, if the open cell holds it (`FREE`).
    pub fn free(&self, extent: ExtentId) -> Result<()> {
        let admitted = self.admit(Changing::Contents)?;
        admitted.local().free(extent).map_err(errno)?;
        // cladefs: its pages, unless another open cell that asked for them holds it still, which
        // reads them through itself (TASK-79.7): one that holds it reads them anew.
        let Some(cached) = self.host.cache().freed(&self.local, self.arenas.of(extent), extent)
        else {
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
            changes.record(range.start, range.end - range.start, WindowChangeKind::Freed);
        }
        Ok(())
    }

    /// Commits what was written and freed, and `root` as the open cell's root if given, uploaded
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
    pub async fn frame_open_segment(&self) -> Result<()> {
        self.local.close_segment().map_err(errno)?;
        self.commit_local(None).await?;
        self.local.wait_framed().await.map_err(errno)
    }

    /// Makes the commits ordered so far local.
    pub async fn flush(&self) -> Result<()> {
        self.local.flush().await.map_err(errno)
    }

    /// cladefs: `Ok` if the open cell takes a request changing `what` of it: the one gate every
    /// such request passes first, each refused for the same reasons, the first that holds: the open
    /// cell closed, halted ([`crate::halt`]), which serves nothing more; its role ([`CellAccess`]);
    /// or its uploader failed for good ([`OpenCell::uploading`]), whose open cell's commits, and
    /// what they would hold, are its cell's no more (doc-4: `EROFS` on `WRITE`, `FREE` and `COMMIT`
    /// once another host took the head). Requests passing their own checks, each its own, let a
    /// halted open cell resize (TASK-611), a fenced one commit (TASK-550) and make versions
    /// (TASK-612).
    pub(super) fn admit(&self, what: Changing) -> Result<Admitted<'_>> {
        if self.closed {
            return Err(Errno::EBADF);
        }
        self.halt.check()?;
        let access = self.access();
        match what {
            Changing::Contents => access.may_write(),
            Changing::Capacity => access.may_resize(),
            Changing::Cell => access.may_manage(),
        }?;
        self.uploading()?;
        Ok(Admitted(self))
    }

    /// cladefs: `Ok` unless the open cell's uploader stopped, failed: the open cell's commits are
    /// its cell's no more, as once a later placement took its cell (`EROFS`, TASK-364), so nothing
    /// changes it from then on ([`OpenCell::admit`]).
    fn uploading(&self) -> Result<()> {
        match self.uploader.as_ref().and_then(|uploader| uploader.failed()) {
            Some(failure) => Err(upload_errno(failure)),
            None => Ok(()),
        }
    }

    /// Makes the commits ordered so far uploaded, if the open cell has an uploader; local, if not.
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
    /// latest: what [`OpenCell::uploaded`] waits for. It blocks on the open cell's I/O.
    pub async fn make_local(&self) -> Result<u64> {
        let epoch = self.local.last_commit().map_err(errno)?;
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

    /// cladefs: what the open cell's uploader has done, and has left to do, if it has one
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
        self.local.flush_pending()
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
    /// children are its phases: `commit.seal` and `commit.write`, the open cell's; then, local or
    /// uploaded, `store.flush`, and `commit.upload_wait` as it waits for object storage.
    async fn commit_with(&self, root: Option<&[u8]>, commit: Commit) -> Result<()> {
        self.commit_request(root, CommitRequest::Commit(commit)).await
    }

    /// Commits for `request`, tracing and counting the commit (board 7409).
    async fn commit_request(&self, root: Option<&[u8]>, request: CommitRequest) -> Result<()> {
        let span = trace::Span::start("store.commit");
        span.set_cell(self.cell());
        span.set(trace::LEVEL, request.level());
        span.run_async(async {
            let admitted = self.admit(Changing::Contents)?;
            let started = std::time::Instant::now();
            let committed = self.commit_and_wait(&admitted, root, request).await;
            stats::count_since(Counter::Commits, Counter::CommitNs, started);
            committed
        })
        .await
    }

    /// Commits, and waits as `request` says: for the uploader's lag, the host's disk, or object
    /// storage; the commit's phases, each counted ([`stats::Stats::commits`]).
    async fn commit_and_wait(
        &self,
        admitted: &Admitted<'_>,
        root: Option<&[u8]>,
        request: CommitRequest,
    ) -> Result<()> {
        /// What a commit waits for: a level of [`Commit`]'s, or fast WAL (doc-29).
        enum Waiting {
            Commit(Commit),
            FastWal,
        }
        let committing = std::time::Instant::now();
        admitted.local().commit_ordered(root).await.map_err(errno)?;
        stats::add_since(Counter::CommitStoreNs, committing);
        let waiting = std::time::Instant::now();
        // cladefs: in a fast-WAL cell (doc-29), the guest's fsync waits for its configured
        // fast-WAL fsync level, and its sync family waits for fast-WAL durability (appendable
        // zonal store or cold regional bucket). An ordering-only pre-barrier (`WRITE_BARRIER`,
        // board 7409) orders prior writes ahead of the write it precedes and never upgrades to
        // `fast_wal`.
        let fast_wal = self.uploader.as_ref().and_then(|uploader| uploader.fast_wal_fsync());
        let commit = match (request, fast_wal) {
            (CommitRequest::Barrier, _) => Waiting::Commit(Commit::Ordered),
            (CommitRequest::Commit(Commit::Uploaded), Some(_)) => Waiting::FastWal,
            (CommitRequest::Commit(Commit::Ordered), Some(fsync)) => match fsync {
                bucket_cell::layout::Fsync::Ordered => Waiting::Commit(Commit::Ordered),
                bucket_cell::layout::Fsync::FastWal => Waiting::FastWal,
                bucket_cell::layout::Fsync::Replicated => {
                    return Err(Errno::ENOTSUP);
                }
            },
            (CommitRequest::Commit(Commit::Local), Some(_)) => Waiting::FastWal,
            (CommitRequest::Commit(commit), _) => Waiting::Commit(commit),
        };
        let waited = match commit {
            Waiting::FastWal => self.make_fast_wal().await,
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

    /// cladefs: returns once the open cell's commits are within the lag its uploader allows
    /// (doc-12), which every commit level waits for: a lost host costs what is past the bucket, no
    /// more, and the host's disk holds no more (doc-14). Fails once the uploader failed for good,
    /// as no commit made since is its cell's (TASK-550, TASK-582).
    async fn within_lag(&self) -> Result<()> {
        let Some(uploader) = self.uploader.as_ref() else { return Ok(()) };
        let epoch = self.local.last_commit().map_err(errno)?;
        upload_wait(epoch).run_async(uploader.lag(epoch)).await.map_err(upload_errno)
    }

    /// cladefs: makes the open cell's commits so far durable in its fast WAL (doc-29): staged in
    /// memory/cache with zero local disk `fsync`s, appended to the appendable zonal bucket (or
    /// uploaded to the cold regional bucket if degraded), and within the lag the uploader allows.
    /// Returns immediately whenever the cold head acknowledges `epoch`, even if acknowledgement
    /// arrives after lag waiting begins (Hotbed review 121, board 12179).
    async fn make_fast_wal(&self) -> Result<()> {
        let epoch = self.make_local().await?;
        let Some(uploader) = self.uploader.as_ref() else { return Ok(()) };
        let waited = async {
            uploader.fast_wal(epoch).await?;
            uploader.fast_wal_lag(epoch).await
        };
        upload_wait(epoch).run_async(waited).await.map_err(upload_errno)
    }

    /// The root as of the last commit, empty if none set one (`ROOT`).
    pub fn root(&self) -> Result<Vec<u8>> {
        self.local.root().map_err(errno)
    }

    /// Creates the DAX window of `size` bytes that [`map`](Self::map) maps extents into, each
    /// change to whose mappings is made in `second_stage`, if given, rather than in this process's
    /// memory: the extent daemon's connection to the VM process (`crate::cladefsd`), which makes it
    /// in the VM's memory. The guest's faults on the window must be passed to
    /// [`DaxWindow::fault`], as the daemon passes those its VM process reports. An open cell has
    /// one window. cladefs: it refuses every write; a guest takes one that copies on write
    /// ([`create_copying_dax_window`](Self::create_copying_dax_window)).
    pub fn create_dax_window(
        &self,
        size: u64,
        second_stage: Option<Arc<dyn SecondStage>>,
    ) -> Result<DaxWindow, Error> {
        self.install_dax(|| Ok(Dax::new_on(&self.host, self.arenas.own(), size, second_stage)?))
    }

    /// cladefs: creates the DAX window as [`create_dax_window`](Self::create_dax_window) does, one
    /// whose `second_stage` copies on write (backlog doc-13): a write to a page gives the page a
    /// private copy there, its VM process's memory, which it bounds, and the daemon knows nothing
    /// of (TASK-501, doc-26). A cell whose extents can't change, a version or a public base, takes
    /// none (as [`CellAccess::may_write`]).
    pub fn create_copying_dax_window(
        &self,
        size: u64,
        second_stage: Arc<dyn SecondStage>,
    ) -> Result<DaxWindow, Error> {
        let _admitted = self.admit(Changing::Contents)?;
        self.install_dax(|| Ok(Dax::copying_on(&self.host, self.arenas.own(), size, second_stage)?))
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
        let change = WindowChangeKind::Map { extent, offset, result };
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
        let source: Arc<dyn Source> = cached;
        let mut dax = self.dax.lock(&self.halt)?;
        let dax = dax.as_mut().ok_or(Errno::ENOSYS)?;
        if within > 0 {
            dax.map(window_offset, &source, offset, within);
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
    ) -> Result<(Arc<CachedExtent>, u64)> {
        self.dax_range_checked(window_offset, len)?;
        // cladefs: an open extent maps nowhere: a guest's bug, counted, as a tripwire
        // (`open_maps_refused`).
        let extent_len = match self.local.extent(extent).map_err(errno)? {
            Some((_, true)) => return Err(refuse_open_map(extent)),
            Some((len, false)) => len,
            None => return Err(Errno::ENOENT),
        };
        let unit = crate::dax::map_unit();
        let end = offset.checked_add(len).ok_or(Errno::EINVAL)?;
        if !offset.is_multiple_of(unit) {
            return Err(Errno::EINVAL);
        }
        let cached = self.cached(extent, extent_len).await?;
        let within = end.min(extent_len.next_multiple_of(unit)).max(offset);
        Ok((cached, within - offset))
    }

    /// cladefs: maps each of `mappings`, in order, as [`OpenCell::map`] or
    /// [`OpenCell::map_zeros_at`] would, in one change of the window (`MAP_MANY`): the first
    /// that fails fails the call, those before it mapped. A snapshot's window's mappings
    /// ([`OpenCell::window_mappings`]) are mapped again so as its VM is restored, where a `MAP`
    /// each took a change of their own (TASK-92.14).
    pub async fn map_many(&self, mappings: &[WindowMapping]) -> Result<()> {
        self.map_many_with(mappings, false).await
    }

    /// cladefs: [`OpenCell::map_many`], warming each mapped extent range in the daemon's
    /// [`ExtentCache`] before installing the mappings (`MAP_MANY_WARM`, board 11681).
    pub async fn map_many_warm(&self, mappings: &[WindowMapping]) -> Result<()> {
        self.map_many_with(mappings, true).await
    }

    async fn map_many_with(&self, mappings: &[WindowMapping], warm: bool) -> Result<()> {
        let start = std::time::Instant::now();
        // What each maps, as far as the first that fails. Pre-size to `mappings.len()` so a large
        // `MAP_MANY` avoids 2x Vec growth overhead above `MAP_MANY_HELD` (TASK-614, board 2624).
        let mut pieces = Vec::with_capacity(mappings.len());
        let mut failed = None;
        for (at, mapping) in mappings.iter().enumerate() {
            let WindowMapping { window_offset, len, contents } = *mapping;
            let mapped = match contents {
                MappingContents::Extent { extent, offset } => {
                    match self.mapped_of(extent, offset, len, window_offset).await {
                        Ok((cached, within)) => {
                            let warmed = if warm && within > 0 {
                                cached.warm(offset, within).await
                            } else {
                                Ok(())
                            };
                            warmed.map(|()| {
                                if within > 0 {
                                    let source: Arc<dyn Source> = cached;
                                    pieces.push(DaxMapping {
                                        offset: window_offset,
                                        len: within,
                                        source: Some((source, offset)),
                                    });
                                }
                                if len > within {
                                    let (offset, len) = (window_offset + within, len - within);
                                    pieces.push(DaxMapping { offset, len, source: None });
                                }
                            })
                        }
                        Err(errno) => Err(errno),
                    }
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
        // Each recorded as its own MAP would have it, the one that failed too, but zeros'. When no
        // refused accesses wait to be matched, only the tail of `WindowChanges::MOST` entries can
        // remain in the ring buffer (avoiding millions of wasted `Instant::now()` calls on a large
        // `MAP_MANY`).
        let tried = failed.map_or(mappings.len(), |(at, _)| at + 1);
        let mut changes = self.changes.lock(&self.halt)?;
        let record_from = if changes.has_refused() {
            0
        } else {
            tried.saturating_sub(crate::dax::WindowChanges::MOST)
        };
        let mut maps = 0;
        for (at, mapping) in mappings[..tried].iter().enumerate() {
            let result = match failed {
                Some((failed, errno)) if failed == at => Err(errno),
                _ => Ok(()),
            };
            let is_extent = matches!(mapping.contents, MappingContents::Extent { .. });
            if is_extent {
                maps += 1;
            }
            if at < record_from {
                continue;
            }
            let change = match mapping.contents {
                MappingContents::Extent { extent, offset } => {
                    WindowChangeKind::Map { extent, offset, result }
                }
                MappingContents::Zeros if result.is_ok() => WindowChangeKind::Zeros,
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
        self.changes.lock(&self.halt)?.record(window_offset, len, WindowChangeKind::Zeros);
        dax.map_zeros(window_offset, len);
        Ok(())
    }

    /// The DAX window's mappings, in order: what a snapshot of the VM records, to map them again
    /// where it is restored, at the same offsets, with [`OpenCell::map_many`]. Without a
    /// window, none. cladefs: a VM process with private pages, which no mapping names the bytes
    /// of, asks for none, as it refuses its snapshot then (TASK-203, doc-26).
    pub fn window_mappings(&self) -> Result<Vec<WindowMapping>> {
        let dax = self.dax.lock(&self.halt)?;
        Ok(dax.as_ref().map_or_else(Vec::new, Dax::mappings))
    }

    /// cladefs: the DAX window's mappings, as [`OpenCell::window_mappings`] has them, its private
    /// pages or not: what a log says of a page.
    fn mapped(&self) -> Result<Vec<WindowMapping>> {
        let dax = self.dax.lock(&self.halt)?;
        Ok(dax.as_ref().map_or_else(Vec::new, Dax::mappings))
    }

    /// cladefs: notes the page at `window_offset` refused `what` (a guest's access), and returns
    /// what the window maps there, the extent's state here and in the open cell, and the page's
    /// latest changes, said for a log: why the access was refused (TASK-99). The next change of
    /// the window covering the page is logged too, with what it did, which the refusal met
    /// (TASK-44.12).
    pub fn refused(&self, window_offset: u64, what: &str) -> String {
        let mapped = self.describe_mapping(window_offset);
        let Ok(mut changes) = self.changes.lock(&self.halt) else {
            return format!("{mapped}; the open cell is halted");
        };
        let history = changes.of(window_offset);
        changes.refuse(window_offset, what, mapped.clone());
        format!("{mapped}; the page's latest MAPs and UNMAPs: {history:#?}")
    }

    /// cladefs: what the window maps at `window_offset`, and the extent's state here and in the
    /// open cell, said for a log.
    fn describe_mapping(&self, window_offset: u64) -> String {
        let Ok(mappings) = self.mapped() else {
            return "the open cell is halted".into();
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
        let stored = self.local.extent(extent).map(|stored| {
            stored.map(|(len, open)| {
                format!("{len:#x} bytes, {}", if open { "open" } else { "sealed" })
            })
        });
        format!(
            "{extent:?} from {offset:#x}, {:#x} bytes from {:#x}; in the cell: {stored:?}",
            mapping.len, mapping.window_offset,
        )
    }

    /// Forks the open cell as [`OpenCell::fork`] does, but only once nothing is open
    /// ([`OpenCell::is_quiescent`]), and no write comes between that and the fork, which would
    /// seal it under its writer: `EBUSY` otherwise. So a running VM's open cell forks,
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

    /// cladefs: makes a version of the cell (doc-18, doc-20): a name for its state as of its
    /// last commit, once nothing is open, as [`OpenCell::fork_quiescent`] checks. One local
    /// commit closing its segment, durable here when this returns; nothing goes to the bucket on
    /// the way: the cell's head names it once its segment is uploaded, behind. Of the versions,
    /// the newest `keep_last` are kept then, if given, the rest dropped. Returns its ID.
    /// `ENOTSUP` without an uploader, which names versions.
    pub async fn make_version(&self, keep_last: Option<u32>) -> Result<u64, Error> {
        let admitted = self.admit(Changing::Cell)?;
        // cladefs: its versions counted first, which awaits, then committed at rest, which holds
        // the open cell's opening until it is (TASK-685.3).
        self.within_versions(keep_last).await?;
        let id = self.at_rest(&admitted, self.commit_version(&admitted, keep_last, true)).await?;
        let named = self.versions().await.into_iter().map(|version| version.id).collect();
        host_cell::made_version_on(&self.host, self.cell(), id, &self.path, &named);
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

    /// cladefs: commits a version as [`OpenCell::make_version`] does, of the open cell quiescent if
    /// `quiescent` (`LocalCell::commit_version`), and returns its ID.
    async fn commit_version(
        &self,
        admitted: &Admitted<'_>,
        keep_last: Option<u32>,
        quiescent: bool,
    ) -> Result<u64, Error> {
        admitted.uploader().ok_or(Errno::ENOTSUP)?;
        let id = rand::random::<u64>().max(1);
        let version = bucket_cell::layout::NewVersion { id, keep_last };
        admitted.local().commit_version(version, quiescent).await.map_err(|err| match err
            .is::<NotQuiescent>(
        ) {
            true => Error::new(anyhow::Error::new(Errno::EBUSY).context(err)),
            false => Error::new(err),
        })?;
        Ok(id)
    }

    /// cladefs: the versions of the cell, oldest first: those its head names, uploaded,
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
        let admitted = self.admit(Changing::Cell)?;
        let uploader = admitted.uploader().ok_or(Errno::ENOTSUP)?;
        let dropped = uploader.drop_versions(ids).await.map_err(upload_errno);
        // cladefs: forgotten as made here, dropped or not: a drop that failed may have taken
        // effect, and one that didn't is the caller's to retry, which needs no open cell made here
        // (TASK-610).
        host_cell::dropped_versions_on(&self.host, self.cell(), ids);
        Ok(dropped?)
    }

    /// cladefs: shares the version `id`, if `shared`, or no longer: forks of it into other tenants'
    /// cells are made while it is shared (`EACCES` otherwise), those made living on. Once every
    /// commit ordered so far is uploaded: a version made is named by the cell's head only once its
    /// segment is (`make_version`), by this open cell's writer or by the one an open cell let go of
    /// and opened again took the cell with, so that a version just made is shared, not refused as
    /// unknown (`ENOENT`).
    pub async fn share_version(&self, id: u64, shared: bool) -> Result<(), Error> {
        let admitted = self.admit(Changing::Cell)?;
        let uploader = admitted.uploader().ok_or(Errno::ENOTSUP)?;
        self.make_uploaded().await?;
        Ok(uploader.share_version(id, shared).await.map_err(upload_errno)?)
    }

    /// cladefs: the version of its cell it is at, if it is one ([`OpenCell::open_version_at`]).
    pub fn version(&self) -> Option<u64> {
        self.local.version()
    }

    /// cladefs: opens the version `id` of the cell at `path`, which must not exist, as the daemon
    /// opens a version for forks of it ([`OpenCell::create_at_version`]), once the version is
    /// uploaded: for tests of forks of versions.
    #[cfg(test)]
    pub fn open_version_at(&self, path: impl AsRef<Path>, id: u64) -> Result<(), Error> {
        let path = path.as_ref();
        let kind = self.local.kind().ok_or(Errno::EINVAL)?;
        let url = self.local.bucket().ok_or(Errno::EINVAL)?;
        Self::create_at_version(path, (url, kind), self.local.description(), id)
    }

    /// cladefs: makes a cell directory at `path` on `host`, of the version `id` of `description`,
    /// running `extra` before removing the `unmade` marker so a failure in `extra` (such as copying
    /// attached bases or public markers, board 5388) discards the directory rather than leaving it
    /// marked made.
    pub fn create_at_version_with_on(
        host: &crate::Host,
        path: &Path,
        (url, kind): (&str, Kind),
        description: local_cell::Description,
        id: u64,
        extra: impl FnOnce() -> Result<(), Error>,
    ) -> Result<(), Error> {
        host_fs::create_dir(path).map_err(|err| match err.kind() {
            std::io::ErrorKind::AlreadyExists => Error::new(Errno::EEXIST),
            _ => Error::new(anyhow::Error::new(err).context(format!("making {}", path.display()))),
        })?;
        let made =
            local_cell::create_host_at_version_on(host.local(), path, description, url, kind, id)
                .map_err(Error::new)
                .and_then(|()| extra())
                .and_then(|()| local_cell::made_host_on(host.local(), path).map_err(Error::new));
        if let Err(err) = made {
            crate::host_cell::discard_on(host, path);
            return Err(err);
        }
        Ok(())
    }

    /// cladefs: makes a cell directory at `path`, which must not exist, of the version `id` of the
    /// cell `description` names, of `kind`, kept in the bucket at `url`: what the daemon opens a
    /// version in for forks of it on a host, nothing writing it.
    #[cfg(not(madsim))]
    pub fn create_at_version(
        path: &Path,
        (url, kind): (&str, Kind),
        description: local_cell::Description,
        id: u64,
    ) -> Result<(), Error> {
        Self::create_at_version_with_on(
            crate::Host::process(),
            path,
            (url, kind),
            description,
            id,
            || Ok(()),
        )
    }

    /// cladefs: makes a version, uploaded, and opens it at `path` as [`OpenCell::open_version_at`]
    /// does: for tests of forks of versions.
    #[cfg(test)]
    pub async fn uploaded_version_at(&self, path: impl AsRef<Path>) -> Result<u64, Error> {
        let id = self.make_version(None).await?;
        self.make_uploaded().await?;
        host_fs::may_block(|| self.open_version_at(path, id))?;
        Ok(id)
    }

    /// cladefs: runs `then` once nothing is open, and none comes between: `EBUSY` with an extent
    /// open. Private pages, whose bytes no extent holds (TASK-203, backlog doc-13), are its VM
    /// process's, which refuses its snapshot while it holds any, before it asks for the version
    /// (doc-26).
    async fn at_rest<T>(
        &self,
        _admitted: &Admitted<'_>,
        then: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        let _admitted = self.admit(Changing::Cell)?;
        let _opening = self.opening.write(&self.halt).await?;
        if !self.is_quiescent()? {
            let open = "an extent written since the last commit is open";
            return Err(Error::new(anyhow::Error::new(Errno::EBUSY).context(open)));
        }
        then.await
    }

    /// cladefs: forks the open cell as [`OpenCell::fork_quiescent`] says. A VM process whose window
    /// holds private pages, whose bytes are the VM's, held by no extent, asks for no fork: it, or
    /// a VM snapshot's version, would name their sources, and a VM restored from it read their old
    /// bytes (TASK-203, backlog doc-13, doc-26). The guest's freeze writes them back and rebinds
    /// them first.
    async fn fork_at_rest(&self, path: &Path, cell: CellId) -> Result<(), Error> {
        let admitted = self.admit(Changing::Cell)?;
        self.fork_span(cell).run_async(self.fork_at_version(&admitted, path, cell, true)).await
    }

    /// cladefs: whether the open cell holds what `other` does, as of their last commits: the same
    /// root, and the same extents, of the same lengths, its ancestors' among them. As an open cell
    /// a version was taken of does, written nothing since, and a fork of the version that wrote
    /// nothing: what a VM restored from a snapshot at the version may run on, its memory the
    /// version's (TASK-194).
    pub fn holds_what(&self, other: &OpenCell) -> Result<bool> {
        let held = |local: &LocalCell<HostDir>| -> anyhow::Result<_> {
            Ok((local.root()?, local.extents()?))
        };
        Ok(held(&self.local).map_err(errno)? == held(&other.local).map_err(errno)?)
    }

    /// Whether nothing is open: no extent written since the last commit, which a fork of the
    /// open cell as of that commit would not hold; as with its guest's filesystem frozen, a version
    /// a snapshot of its VM is taken at.
    pub fn is_quiescent(&self) -> Result<bool> {
        // cladefs: not, as far as anyone knows, once a panic went through it (`crate::halt`).
        self.halt.check()?;
        self.local.has_open().map(|open| !open).map_err(errno)
    }

    /// Unmaps `len` bytes of the DAX window at `window_offset` (`UNMAP`).
    pub fn unmap(&self, window_offset: u64, len: u64) -> Result<()> {
        let mut dax = self.dax.lock(&self.halt)?;
        let dax = dax.as_mut().ok_or(Errno::ENOSYS)?;
        dax.check_range(window_offset, len)?;
        self.changes.lock(&self.halt)?.record(window_offset, len, WindowChangeKind::Unmap);
        dax.unmap(window_offset, len);
        Ok(())
    }

    /// The sealed `extent`, of `len` bytes, in the cache: read from this open cell if no other open
    /// cell has it cached.
    async fn cached(&self, extent: ExtentId, len: u64) -> Result<Arc<CachedExtent>> {
        self.arenas.cached(&self.local, self.fetcher(), extent, len).await
    }

    /// cladefs: makes the cell public, and those it inherited from (TASK-43): a base,
    /// which every tenant forks, whose extents' pages, and its ancestors', are of the default
    /// arena of physical memory, which every tenant's VM processes hold read-only.
    /// Open cells opened since take it so; a tenant opens no public open cell itself, only forks of
    /// it.
    pub fn make_public(&self) -> Result<()> {
        let admitted = self.admit(Changing::Cell)?;
        cell_dir::mark_public_on(self.host.local(), admitted.local().dir().path(), &self.ancestry())
    }

    /// cladefs: the cell, and those it inherited from, nearest first.
    pub fn ancestry(&self) -> Vec<u64> {
        let inherited = self.forked_from.iter().map(|(cell, _)| *cell);
        std::iter::once(self.cell().id).chain(inherited).collect()
    }

    /// cladefs: whether the cell is public ([`OpenCell::make_public`]).
    pub fn is_public(&self) -> bool {
        self.arenas.public.contains(&self.cell().id)
    }

    /// cladefs: the arena of physical memory its own extents' pages are of
    /// ([`Opening::arena`]): its tenant's, or the default.
    pub fn arena(&self) -> ArenaId {
        self.arenas.own()
    }
}

/// cladefs: that an extent is reachable from a published generation (TASK-234): a public open cell,
/// a generation or a base, holds it, so that every tenant attaching or forking it may read it. Only
/// an open cell makes one, having found so ([`Arenas::public_reach`]): its field is this
/// module's. The extent cache moves a tenant's copy of an extent to the default arena, which every
/// VM process maps, only with one naming that extent (`cache::CachedIn::Public`).
pub struct PublicReach(ExtentId);

impl PublicReach {
    /// The extent reachable.
    pub fn extent(&self) -> ExtentId {
        self.0
    }
}

/// cladefs: the arenas of physical memory an open cell's extents' pages are of (TASK-43).
#[derive(Clone)]
pub struct Arenas {
    host: Arc<crate::Host>,
    /// The open cell's, its tenant's, held for as long as the open cell and what reads into the
    /// extent cache for it, which clone these, allocate in it (TASK-79.36); or none, the default.
    own: Option<Arc<Arena>>,
    /// The public cells of its own and those it was forked from, as its directory marks them,
    /// whose extents' pages are of the default arena.
    public: Arc<std::collections::BTreeSet<u64>>,
}

impl Arenas {
    /// The open cell's arena.
    fn own(&self) -> ArenaId {
        self.own.as_ref().map_or(ArenaId::DEFAULT, |arena| arena.id())
    }

    /// The arena `extent`'s pages are of.
    fn of(&self, extent: ExtentId) -> ArenaId {
        if self.public.contains(&extent.cell) { ArenaId::DEFAULT } else { self.own() }
    }

    /// The sealed `extent`, of `len` bytes, of `local`, the cell of the open cell these are the
    /// arenas of, in the cache: read from `local`, or through `fetcher`, if no other open cell has
    /// it cached. What the open cell reads, and what its fork's prefetch reads ahead of its guest
    /// (`crate::evidence`), read through.
    pub async fn cached(
        &self,
        local: &Arc<LocalCell<HostDir>>,
        fetcher: Arc<crate::bucket::HostFetcher>,
        extent: ExtentId,
        len: u64,
    ) -> Result<Arc<CachedExtent>> {
        let cached_in = match self.public_reach(local, extent) {
            Some(reach) => CachedIn::Public(reach),
            None => CachedIn::Arena(self.of(extent)),
        };
        self.host.cache().cached(local, cached_in, Some(fetcher), extent, len).await
    }

    /// cladefs: that `extent` is reachable from a published generation, if it is: `local`'s cell
    /// is public, a generation or a base its directory marks so, and holds it sealed (TASK-234).
    fn public_reach(&self, local: &LocalCell<HostDir>, extent: ExtentId) -> Option<PublicReach> {
        if !self.public.contains(&local.description().cell.id) {
            return None;
        }
        let held = local.extent(extent);
        matches!(held, Ok(Some((_, false)))).then_some(PublicReach(extent))
    }
}
