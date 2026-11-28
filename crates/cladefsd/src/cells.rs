// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's cells, by the paths that name them: those VM processes' connections have open,
//! and what the daemon does to a cell by its path, for a connection or the control API
//! ([`super::control`]), holding it meanwhile, so that no connection opens it then.
//!
//! What the control API asks for is a future (TASK-43.6): its waits, for uploads, a guest's
//! answer, and a connection's letting a cell go, hold no thread, and its cells' I/O runs on the
//! runtime's blocking threads ([`blocking`]), as do opening and closing a cell, which waits for
//! another process's lock of it, and its cell's take from object storage.
//!
//! Who holds a cell, and what is open of it, is the daemon's one table's (`crate::cell_table`,
//! TASK-160.8), each hold a value that lets the cell go as it is dropped. cladefs: a cell open is
//! one instance ([`OpenCell`]): its writer's, if a connection writes it, which the daemon's work
//! through it shares; or the one the cells a base is attached to share (backlog doc-10). No writer
//! opens a cell others have open (`EBUSY`).

use crate::Daemon;
use crate::storage::{daemon_bucket_on, part_cache, upload_closed, upload_interval};
use bucket_cell::cell::Kind;
use cladefs_core::cell_table::{self, CellTable};
use cladefs_core::open_cell::{Opening, release};
use cladefs_core::pages::Arena;
use cladefs_core::{CellId, CellOf, Errno, ExtentInfo, OpenCell};
use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

/// The daemon's cells, by their keys ([`cell_dir_key`]): who holds each, a writer (a VM process's
/// connection, or a cell embedded in the daemon's process) or the daemon working on it, and what
/// is open of it, the cell its writer has open or a kept cell kept open for forks.
pub(super) type Table = CellTable<dyn CellWriter, OpenCell, Kept>;

/// A cell held (`daemon.table`): by a writer, or by the daemon working on it, which no writer
/// opens meanwhile; until it is dropped.
pub(super) type Hold = cell_table::Hold<dyn CellWriter, OpenCell, Kept>;

/// cladefs: what writes a cell the daemon holds (board 13909): a VM process's connection
/// ([`crate::server::Connection`]), or a cell the daemon's own process has open, with no VM process
/// ([`crate::embedded::EmbeddedCell`]). What the daemon's work on a cell asks of its writer.
pub(super) trait CellWriter: cell_table::Writer {
    /// Its cell, once open.
    fn cell(&self) -> Option<&Arc<OpenCell>>;
    /// Sets its cell's capacity to `capacity` bytes (the control API's `ResizeCell`), as its user,
    /// a guest or the process, agrees.
    fn resize(
        &self,
        capacity: u64,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Errno>> + Send + '_>>;
    /// It, if it is a VM process's connection: for tests of what a connection does.
    #[cfg(test)]
    fn connection(self: Arc<Self>) -> Option<Arc<crate::server::Connection>>;
}

/// cladefs: the VM process's connection writing the cell `key` names, if one does (tests).
#[cfg(test)]
pub(super) fn connection_writing(key: &Path) -> Option<Arc<crate::server::Connection>> {
    table().writer(key).and_then(CellWriter::connection)
}

/// Holds the cell `key` names on `daemon` for `connection` to write it, once a connection whose VM
/// process is gone lets it go, waiting at most [`CLOSING_WAIT`] (`EBUSY`), holding no thread
/// meanwhile: `EBUSY` while a connection not going writes it. cladefs: the cell kept open of it
/// for forks, which the hold takes, closed, once the forks of it under way are done.
pub(super) async fn hold_to_write(
    daemon: &Arc<Daemon>,
    key: &Path,
    connection: Weak<dyn CellWriter>,
) -> Result<Hold, Errno> {
    let (hold, kept) = daemon.table.hold_to_write(key, connection, CLOSING_WAIT).await?;
    close_kept(daemon, kept).await;
    Ok(hold)
}

/// How long holding a cell waits for a connection whose VM process is gone to let it go: as a VM
/// restored from a snapshot at once opens the cell its VM had (TASK-39.1).
pub(super) const CLOSING_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// cladefs: how long holding a version's cell waits for its holder (TASK-443.6): as long as it
/// holds it. Its holders are the daemon's own work, which ends: a restore opening it from the
/// bucket for the restores beside it, which waited [`CLOSING_WAIT`] at most before and failed
/// `EBUSY` (11 of 20 restores of one version, its opening taking 10.9 s from a bucket 600 ms
/// away); a sync of it; its close, once no restore forked it for a while.
const VERSION_WAIT: std::time::Duration = std::time::Duration::MAX;

/// The path that names the cell at `path` in the daemon's [`table`]: whichever way a request
/// names it.
pub(super) fn cell_dir_key(path: &Path) -> PathBuf {
    local_cell::upload::blocking(|| {
        if let Ok(path) = host_fs::canonicalize(&path) {
            return path;
        }
        // A cell not yet made: where it will be.
        match (path.parent().map(host_fs::canonicalize), path.file_name()) {
            (Some(Ok(parent)), Some(name)) => parent.join(name),
            _ => path.to_owned(),
        }
    })
}

/// cladefs: holds the cell at `path` on `daemon` for the daemon's FUSE mount of cells, which
/// attaches it (TASK-475.3): until what it returns is dropped, as the mount detaches it, a VM
/// process opening it, or the control API working on it, fails with `EBUSY` at once.
#[cfg(all(feature = "mount", target_os = "linux", not(madsim)))]
pub(crate) async fn hold_mounted(daemon: &Arc<Daemon>, path: &Path) -> Result<MountHold, Errno> {
    let (hold, kept) = daemon.table.hold_mounted(&cell_dir_key(path), CLOSING_WAIT).await?;
    close_kept(daemon, kept).await;
    Ok(MountHold(hold))
}

/// cladefs: a cell the daemon's FUSE mount holds ([`hold_mounted`]), let go as it is dropped.
#[cfg(all(feature = "mount", target_os = "linux", not(madsim)))]
pub(crate) struct MountHold(#[allow(dead_code, reason = "held for its drop")] Hold);

/// Holds the cell at `path` for `daemon`, which no connection opens meanwhile.
async fn hold(daemon: &Arc<Daemon>, path: &Path) -> Result<Hold, Errno> {
    hold_within(daemon, path, CLOSING_WAIT).await
}

/// cladefs: holds the cell at `path` for `daemon`, as [`hold`] does, waiting at most `wait`.
async fn hold_within(
    daemon: &Arc<Daemon>,
    path: &Path,
    wait: std::time::Duration,
) -> Result<Hold, Errno> {
    let hold = daemon.table.hold_async(&cell_dir_key(path), wait).await?;
    close_kept(daemon, hold.take_kept()).await;
    Ok(hold)
}

/// cladefs: holds the cell at `path` for `daemon`, as [`hold`] does, but for the cell kept open
/// of it, which it leaves open: to fork it, or open it to ([`kept_or_open`]).
async fn hold_to_fork(daemon: &Daemon, path: &Path) -> Result<Hold, Errno> {
    hold_to_fork_within(daemon, path, CLOSING_WAIT).await
}

/// cladefs: holds the cell at `path` for `daemon` for at most `wait`, leaving any cell kept open
/// of it open (TASK-549, board 2494).
async fn hold_to_fork_within(
    daemon: &Daemon,
    path: &Path,
    wait: std::time::Duration,
) -> Result<Hold, Errno> {
    daemon.table.hold_async(&cell_dir_key(path), wait).await
}

/// cladefs: holds the cell at `path` on `daemon`, which nothing has open (`EBUSY` otherwise), and
/// waits for any in-flight async close/write-back to release its directory claim before scanning
/// files on disk (TASK-698.4, board 11530).
pub(super) async fn hold_cell_alone(daemon: &Arc<Daemon>, path: &Path) -> Result<Hold, Errno> {
    let hold = hold_alone(daemon, path).await?;
    local_cell::unclaimed_on(daemon.host.local(), path).await.map_err(|err| match err.kind() {
        std::io::ErrorKind::WouldBlock => Errno::EBUSY,
        _ => {
            log::warn!("cladefsd: waiting for {} to close: {err}", path.display());
            Errno::EIO
        }
    })?;
    Ok(hold)
}

async fn hold_alone(daemon: &Arc<Daemon>, path: &Path) -> Result<Hold, Errno> {
    let hold = hold(daemon, path).await?;
    if let Some(cell) = hold.instance() {
        inspect(daemon, cell, |_| ());
        return Err(Errno::EBUSY);
    }
    Ok(hold)
}

/// Runs `work`, which blocks, on one of the runtime's blocking threads, and returns what it
/// returns.
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Errno> + Send + 'static,
) -> Result<T, Errno> {
    // Its work unfinished, the runtime gone: said, and failed.
    crate::run::off_the_runtime(work).await.unwrap_or_else(|err| {
        log::error!("cladefsd: a cell's blocking work: {err}");
        Err(Errno::EIO)
    })
}

/// A cell the daemon works on, and what it lets go of once the cell is closed, if anything,
/// dropped in that order (TASK-79.34): the daemon's hold of its path is let go only once the
/// cell, one the daemon opened, is closed, whoever drops it last, a call or a blocking thread
/// whose work a cancelled call left going.
pub(super) struct HeldCell {
    open_cell: Arc<OpenCell>,
    _then: Option<Then>,
}

/// What a cell the daemon works on lets go of once it is closed ([`HeldCell`]).
enum Then {
    /// The daemon's hold of its path.
    LetGo { _hold: Hold },
    /// cladefs: what says a cell kept open is closed, to what closes it ([`Kept`]).
    SayClosed { _closing: futures::channel::oneshot::Sender<std::convert::Infallible> },
}

impl Borrow<OpenCell> for HeldCell {
    fn borrow(&self) -> &OpenCell {
        &self.open_cell
    }
}

/// A cell the daemon works on ([`HeldCell`]): one it opened, or one open for connections, which
/// it lets go of in a task of its own ([`release_held`]), so that a call cancelled as it lets it go
/// leaves it to close all the same.
struct Here {
    daemon: Arc<Daemon>,
    held: Option<Arc<HeldCell>>,
}

impl Here {
    /// The cell at `path`, opened on a blocking thread ([`open_here`]), which `hold` holds.
    async fn open(daemon: &Arc<Daemon>, path: &Path, hold: Hold) -> Result<Self, Errno> {
        let open_cell = Arc::new(open_here(daemon, path).await?);
        Ok(Self {
            daemon: Arc::clone(daemon),
            held: Some(Arc::new(HeldCell::new(open_cell, Some(hold)))),
        })
    }

    /// `open_cell`, which `hold` holds, if anything does: one open for connections, or opened here.
    fn of(daemon: &Arc<Daemon>, open_cell: Arc<OpenCell>, hold: Option<Hold>) -> Self {
        Self { daemon: Arc::clone(daemon), held: Some(Arc::new(HeldCell::new(open_cell, hold))) }
    }

    /// cladefs: `kept`, a cell kept open for forks ([`keep`]).
    fn kept(daemon: &Arc<Daemon>, kept: Arc<HeldCell>) -> Self {
        Self { daemon: Arc::clone(daemon), held: Some(kept) }
    }

    fn held(&self) -> &Arc<HeldCell> {
        self.held.as_ref().expect("an open cell")
    }
}

impl HeldCell {
    /// `open_cell`, which `hold` holds, if anything does.
    fn new(open_cell: Arc<OpenCell>, hold: Option<Hold>) -> Self {
        Self { open_cell, _then: hold.map(|_hold| Then::LetGo { _hold }) }
    }

    /// cladefs: lets it go: its cell, closed if this was its last reference ([`release`]), and then
    /// what it lets go of once that is closed, in the order its drop would (TASK-79.34), holding no
    /// thread (doc-24, step 9).
    async fn close(self) {
        let HeldCell { open_cell, _then } = self;
        release(open_cell).await;
        drop(_then);
    }
}

/// cladefs: lets `held` go, and closes it ([`HeldCell::close`]) if this was its last reference: as
/// every holder of one lets it go, so that whichever is last closes it, awaited.
async fn release_held(held: Arc<HeldCell>) {
    if let Some(held) = Arc::into_inner(held) {
        held.close().await;
    }
}

/// cladefs: lets `held` go in a task of its own, on `daemon`'s runtime: what lets one go where it
/// can't await its close, as a drop, or a call that may be cancelled. Marks the cell closing
/// synchronously when unwrapping the last `Arc<OpenCell>` so concurrent openers wait instead of
/// failing with `EBUSY` before the close task starts (board 11340).
fn release_held_behind(daemon: &Daemon, held: Arc<HeldCell>) -> tokio::task::JoinHandle<()> {
    if let Some(HeldCell { open_cell, _then }) = Arc::into_inner(held) {
        let closing = Arc::into_inner(open_cell).inspect(|cell| cell.mark_closing());
        return daemon.spawn(async move {
            if let Some(cell) = closing {
                cell.close().await;
            }
            drop(_then);
        });
    }
    daemon.spawn(async {})
}

impl Drop for Here {
    fn drop(&mut self) {
        if let Some(open_cell) = self.held.take() {
            drop(release_held_behind(&self.daemon, open_cell));
        }
    }
}

impl Here {
    /// Closes this handle's cell on `daemon`'s runtime and awaits completion before returning.
    async fn close(mut self) {
        if let Some(open_cell) = self.held.take()
            && let Err(err) = release_held_behind(&self.daemon, open_cell).await
        {
            log::error!("cladefsd: closing a cell: {err}");
        }
    }
}

/// cladefs: how long a kept cell the daemon opened to fork stays open with no fork of it
/// (TASK-92.5): a zygote's restores, which fork its snapshot's kept cell, come together, and the
/// kept cell's opening, attaching and closing, a second or more with object storage a region away,
/// is paid once for them, rather than by each in turn, the last of them waiting for all those
/// before it.
const LINGER: std::time::Duration = std::time::Duration::from_secs(60);

/// cladefs: a kept cell the daemon opened to fork, kept open for the forks after it ([`keep`]): its
/// cell, which they fork at once, as it is; what says it is closed, to what closes it, once the
/// forks forking it are done; and when it was last forked. Kept in the daemon's [`table`], at the
/// key of the cell it is, of which no connection has anything open: what holds the cell's path
/// to work on it, but to fork it, closes the kept cell first ([`hold`], [`hold_to_write`]).
pub(super) struct Kept {
    held: Arc<HeldCell>,
    closed: futures::channel::oneshot::Receiver<std::convert::Infallible>,
    forked: std::time::Instant,
}

/// cladefs: the cell kept open at `key` on `daemon`, if one is, forked now.
fn kept(daemon: &Daemon, key: &Path) -> Option<Arc<HeldCell>> {
    daemon.table.kept(key, |kept| {
        kept.forked = std::time::Instant::now();
        kept.held.clone()
    })
}

/// cladefs: when `open`, kept open at `key` on `daemon`, was last forked, if it is kept still.
fn forked(daemon: &Daemon, key: &Path, open: &Weak<HeldCell>) -> Option<std::time::Instant> {
    let is_open = |kept: &mut Kept| std::ptr::eq(Arc::as_ptr(&kept.held), open.as_ptr());
    daemon.table.kept(key, |kept| is_open(kept).then_some(kept.forked)).flatten()
}

/// cladefs: keeps `open_cell`, opened to fork at the key `hold` holds, open for the forks after it
/// (TASK-92.5).
fn keep(daemon: &Arc<Daemon>, hold: &Hold, open_cell: OpenCell) -> Arc<HeldCell> {
    let (closing, closed) = futures::channel::oneshot::channel();
    // What says it is closed goes after the cell.
    let held = Arc::new(HeldCell {
        open_cell: Arc::new(open_cell),
        _then: Some(Then::SayClosed { _closing: closing }),
    });
    let kept = Kept { held: held.clone(), closed, forked: std::time::Instant::now() };
    match hold.keep(kept) {
        Ok(()) => {
            let idle = let_go_when_idle(
                Arc::downgrade(daemon),
                hold.key().to_owned(),
                Arc::downgrade(&held),
            );
            // On the runtime that lives as long as the daemon, whatever its caller's.
            drop(daemon.spawn(idle));
        }
        // Something open there already: this one closes after its fork, said here.
        Err(_) => log::error!(
            "cladefsd: {} has something open already: the kept opened isn't kept",
            hold.key().display()
        ),
    }
    held
}

/// cladefs: closes `kept`, if given, a cell kept open its holder took, and returns once it is
/// closed: once the forks forking it are done, holding no thread while it waits.
async fn close_kept(daemon: &Daemon, kept: Option<Kept>) {
    let Some(Kept { held, closed, .. }) = kept else { return };
    drop(release_held_behind(daemon, held));
    // Its sender dropped: closed, as said.
    let _ = closed.await;
}

/// cladefs: closes every cell kept open for forks on `daemon`, as the daemon drains (TASK-141.4).
pub(super) async fn close_all_kept(daemon: &Arc<Daemon>) {
    for key in daemon.table.kept_keys() {
        if let Err(errno) = hold(daemon, &key).await {
            log::warn!("cladefsd: closing the kept at {}: {errno}", key.display());
        }
    }
}

/// cladefs: closes `kept`, kept open at `key`, once none has forked it for [`LINGER`], under the
/// daemon's hold of its path, so that no fork opens it anew as it closes; unless something closed
/// it before, as a connection's opening it does, which this then leaves to it.
async fn let_go_when_idle(daemon: Weak<Daemon>, key: PathBuf, kept: Weak<HeldCell>) {
    loop {
        let Some(daemon) = daemon.upgrade() else { return };
        let Some(last) = forked(&daemon, &key, &kept) else { return };
        match LINGER.checked_sub(last.elapsed()).filter(|left| !left.is_zero()) {
            Some(left) => tokio::time::sleep(left).await,
            None => {
                let Ok(hold) = hold_to_fork(&daemon, &key).await else { return };
                if forked(&daemon, &key, &kept).is_some_and(|last| last.elapsed() >= LINGER) {
                    return close_kept(&daemon, hold.take_kept()).await;
                }
            }
        }
    }
}

/// cladefs: closes `kept`, kept open at `key`, if it is kept still, one of its forks having failed.
async fn let_go_failed(daemon: &Daemon, key: &Path, kept: Weak<HeldCell>) {
    if forked(daemon, key, &kept).is_none() {
        return;
    }
    let Ok(hold) = hold_to_fork(daemon, key).await else { return };
    if forked(daemon, key, &kept).is_some() {
        close_kept(daemon, hold.take_kept()).await;
    }
}

/// `open_cell`'s capacity, and the bytes it holds.
pub(super) fn capacity(open_cell: &OpenCell) -> Result<(u64, u64), Errno> {
    let info = open_cell.info()?;
    Ok((info.capacity, info.capacity - info.available))
}

/// Sets `open_cell`'s capacity to `capacity` bytes.
async fn set_capacity<S>(open_cell: &Arc<S>, capacity: u64) -> Result<(), Errno>
where
    S: Borrow<OpenCell> + Send + Sync + 'static,
{
    (**open_cell).borrow().set_capacity(capacity).await
}

/// The cell a fork forks ([`Here`]), and whether it is open elsewhere: its writer's, live, or
/// others', rather than opened here.
struct Forked {
    here: Here,
    live: bool,
}

impl Forked {
    async fn close(self) {
        self.here.close().await;
    }
}

/// Returns once every commit of `open_cell` ordered so far is uploaded: its flush, then the wait
/// for its uploads, holding no thread.
pub(super) async fn uploaded<S>(open_cell: &Arc<S>) -> Result<(), Errno>
where
    S: Borrow<OpenCell> + Send + Sync + 'static,
{
    let epoch = (**open_cell).borrow().make_local().await?;
    (**open_cell).borrow().uploaded(epoch).await
}

/// A new cell of `tenant`'s, its ID given by `daemon`'s bucket: `EIO` if the bucket fails, which
/// a VM process's errno can't say, so logged.
pub(super) async fn new_cell(daemon: &Arc<Daemon>, tenant: u64) -> Result<CellId, Errno> {
    let bucket = daemon_bucket_on(daemon)?;
    cladefs_core::host_cell::new_cell(&*bucket, tenant).await.map_err(|err| {
        log::error!("cladefsd: allocating a new cell of tenant {tenant}: {err:#}");
        Errno::EIO
    })
}

/// cladefs: a new cell of `tenant`'s, its ID given by `daemon`'s bucket up front, and its canonical
/// directory on `disk`, `<disk>/cells/<TENANT>/<CELL>`, its tenant's directory made (hotbed
/// question 547): so that its opening creates and takes it there in a single taking (epoch 1),
/// with no staging rename.
pub(super) async fn new_managed_cell(
    daemon: &Arc<Daemon>,
    disk: &Arc<crate::disks::ManagedDisk>,
    tenant: u64,
) -> Result<(PathBuf, CellId), Errno> {
    let cell = new_cell(daemon, tenant).await?;
    let disk = Arc::clone(disk);
    let path = blocking(move || {
        drop(crate::disks::DiskPool::ensure_tenant_dir(&disk, cell.tenant)?);
        Ok(crate::disks::DiskPool::cell_path(&disk, cell))
    })
    .await?;
    Ok((path, cell))
}

/// cladefs: the directory of `cell`, a cell kept on one of `pool`'s disks (`ENOENT` if none).
pub(super) async fn managed_cell_dir(
    pool: &Arc<crate::disks::DiskPool>,
    cell: CellId,
) -> Result<PathBuf, Errno> {
    let pool = Arc::clone(pool);
    blocking(move || pool.find_cell(cell).ok_or(Errno::ENOENT)).await
}

/// The cell a connection has open at `path` on `daemon`, if one has, whose VM process isn't gone.
fn open_by_connection(daemon: &Daemon, path: &Path) -> Option<Arc<OpenCell>> {
    let key = cell_dir_key(path);
    match daemon.table.writer(&key) {
        Some(writer) if writer.going() => None,
        Some(writer) => writer.cell().cloned(),
        None => daemon.table.instance(&key),
    }
}

/// A cell a VM process's opening makes, if there is none ([`open_cell_at`]).
#[derive(Clone, Copy)]
pub(super) struct New {
    /// Its bytes.
    pub capacity: u64,
    /// What it holds.
    pub kind: Kind,
    /// The tenant whose new cell it is the one cell of (TASK-364): the connection's.
    pub tenant: u64,
    /// Pre-allocated cell ID (for direct `DiskPool` placement without double-taking).
    pub cell: Option<CellId>,
    /// Whether the new cell is a standalone host-local ephemeral cell (doc-25).
    pub ephemeral: bool,
}

/// The cell at `path` on `daemon`, made as `new` says if given and it doesn't exist, keeping it in
/// the daemon's bucket, and its own extents' pages in `arena`, which it holds, or in the default
/// arena, without one ([`OpenCell::open_in`]).
pub(super) async fn open_cell_at(
    daemon: &Arc<Daemon>,
    path: &Path,
    new: Option<New>,
    arena: Option<&Arc<Arena>>,
) -> Result<OpenCell, Errno> {
    let arena = arena.cloned();
    attached(daemon, path, async |keeping| {
        let opening = Opening::keeping_on(daemon.host.clone(), keeping).arena(arena);
        match new {
            Some(New { capacity, kind, tenant, ephemeral: true, .. })
                if !local_cell::upload::blocking(|| host_fs::exists(&path)) =>
            {
                OpenCell::create_ephemeral_kind_with(path, capacity, (tenant, kind), opening).await
            }
            Some(New { capacity, kind, tenant, cell, ephemeral: false })
                if !local_cell::upload::blocking(|| host_fs::exists(&path)) =>
            {
                let of = match cell {
                    Some(named) => CellOf::Named(named),
                    None => CellOf::New(tenant),
                };
                OpenCell::create_kind_with(path, capacity, (of, kind), opening).await
            }
            _ => OpenCell::open_with(path, opening).await,
        }
    })
    .await
}

/// Maximum per-cell uploader interval accepted by control APIs (24 hours, hotbed question 534).
const MAX_UPLOAD_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;

/// Validates an optional `upload_interval_ms` from a control RPC at the trust boundary.
pub(crate) fn parse_upload_interval_ms(
    ms: Option<u64>,
) -> Result<Option<std::time::Duration>, Errno> {
    match ms {
        None => Ok(None),
        Some(0) => Err(Errno::EINVAL),
        Some(ms) if ms > MAX_UPLOAD_INTERVAL_MS => Err(Errno::EINVAL),
        Some(ms) => Ok(Some(std::time::Duration::from_millis(ms))),
    }
}

/// The cell at `path`, as `open` opens it with `daemon`'s uploader's options ([`keeping`]),
/// applying any per-cell `upload_interval` configured on `daemon` for `path`.
async fn attached<T>(
    daemon: &Arc<Daemon>,
    path: &Path,
    open: impl AsyncFnOnce(local_cell::upload::Options) -> Result<T, cladefs_core::Error>,
) -> Result<T, Errno> {
    // The daemon's own first, which the cells naming it share (`host_cell::bucket`).
    let mut keeping = mount_keeping(daemon)?;
    if let Some(interval) = daemon
        .upload_intervals
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&cell_dir_key(path))
        .copied()
    {
        keeping.interval = interval;
    }
    open(keeping).await.map_err(|err| errno_of(format_args!("opening {}", path.display()), err))
}

/// cladefs: ensures `daemon`'s bucket is open and returns its uploader/cache options for
/// host-FUSE mounted cells (`crate::fuse_control`, board 11363) so mounted cells share the
/// daemon's `CLADEFSD_PART_CACHE`, fetch budget, and held-parts pool.
pub(crate) fn mount_keeping(daemon: &Arc<Daemon>) -> Result<local_cell::upload::Options, Errno> {
    daemon_bucket_on(daemon)?;
    keeping(daemon)
}

/// How `daemon`'s cells' uploaders keep them in their buckets.
fn keeping(daemon: &Daemon) -> Result<local_cell::upload::Options, Errno> {
    let token = daemon_token()?;
    let (cache, interval) = (part_cache(daemon)?, upload_interval());
    let held = Some(crate::memory::held_parts_on(daemon).clone());
    let (fast_wal_config, fast_wal_large_extent) = match crate::storage::fast_wal_config(daemon)? {
        Some((config, large_extent)) => (Some(config), large_extent),
        None => (None, local_cell::fast_wal::DEFAULT_LARGE_EXTENT),
    };
    Ok(local_cell::upload::Options {
        cache,
        held,
        fetching: Some(cladefs_core::bucket::fetching_on(&daemon.host)),
        token,
        interval,
        upload_closed: upload_closed(),
        fast_wal_config,
        fast_wal_large_extent,
        ..Default::default()
    })
}

/// The cell at `path` as `daemon` opens it to work on it, not a VM.
pub(super) async fn open_here(daemon: &Arc<Daemon>, path: &Path) -> Result<OpenCell, Errno> {
    attached(daemon, path, async |keeping| {
        let opening = Opening::keeping_on(daemon.host.clone(), keeping).guest(false);
        OpenCell::open_with(path, opening).await
    })
    .await
}

/// The errno of `err`, which doing `what` failed with: its own; `EBUSY` if it gave up waiting for
/// another process's lock of the cell; or `EIO`, logged.
pub(super) fn errno_of(what: std::fmt::Arguments<'_>, err: cladefs_core::Error) -> Errno {
    // Another process's lock of the cell, waited for: `EBUSY`, as for a connection's.
    if err.is_busy() {
        log::debug!("cladefsd: {what}: {err:#}");
        return Errno::EBUSY;
    }
    err.errno().unwrap_or_else(|| {
        log::warn!("cladefsd: {what}: {err:#}");
        Errno::EIO
    })
}

/// Makes an empty cell at `path`, which must not exist (`EEXIST`), of `capacity` bytes, and
/// returns its cell: a new one, or `cell`, if given, kept in the daemon's bucket, which the
/// daemon takes once it opens it (`cladefsctl take`); placed at `token` if given ([`place`]).
/// cladefs: made as [`OpenCell::create_placed_on`] makes it, its head taken with `token`, not the
/// daemon's, so that a kept cell placed later than the one on another host that took the cell last
/// takes it over (TASK-266: `ESTALE`, its head taken with the daemon's token before it was
/// placed).
pub(super) async fn create(
    daemon: &Arc<Daemon>,
    path: PathBuf,
    capacity: u64,
    cell: CellId,
    (token, kind, fast_wal, upload_interval, ephemeral): (
        Option<u64>,
        Kind,
        Option<Option<bucket_cell::layout::Fsync>>,
        Option<std::time::Duration>,
        bool,
    ),
) -> Result<CellId, Errno> {
    // Let go of once the work is done.
    let _held = hold(daemon, &path).await?;
    // cladefs: what a making its host died in left, unmade, is no cell: made anew (TASK-196).
    let checked = path.clone();
    let host = daemon.host.clone();
    blocking(move || {
        removed_if_unmade_on(&host, &checked)?;
        if host_fs::exists(&checked) {
            return Err(Errno::EEXIST);
        }
        Ok(())
    })
    .await?;
    if ephemeral {
        if cell.id != 0 || token.is_some() || fast_wal.is_some() || upload_interval.is_some() {
            return Err(Errno::EINVAL);
        }
        return attached(daemon, &path, async |keeping| {
            OpenCell::create_ephemeral_closed_on(
                &daemon.host,
                &path,
                capacity,
                cell.tenant,
                kind,
                &keeping,
            )
            .await
        })
        .await;
    }
    // cladefs: a cell of 0 is a new one, its ID, its cell's too, the bucket's to give: no
    // caller names a new cell; a cell named, one the bucket has, of a cell named (TASK-364).
    let of = match (cell.id, cell.id) {
        (0, 0) => CellOf::New(cell.tenant),
        (_, 0) | (0, _) => return Err(Errno::EINVAL),
        (_, _) => CellOf::Taken(cell),
    };
    // cladefs: the Fast WAL mode its call asked for, which its taking gives the cell's head
    // (doc-29); and any per-cell uploader interval override (hotbed question 534).
    let made = attached(daemon, &path, async |keeping| {
        let interval = upload_interval.unwrap_or(keeping.interval);
        let keeping = local_cell::upload::Options { fast_wal, interval, ..keeping };
        OpenCell::create_placed_on(&daemon.host, &path, (capacity, of, token), kind, keeping).await
    })
    .await?;
    if let Some(interval) = upload_interval {
        daemon
            .upload_intervals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(cell_dir_key(&path), interval);
    }
    Ok(made)
}

/// Makes a cell at `path` on `daemon`, which must not exist (`EEXIST`), of `capacity` bytes,
/// holding a filesystem of the container image at `image`.
pub(super) async fn import(
    daemon: &Arc<Daemon>,
    path: PathBuf,
    capacity: u64,
    image: PathBuf,
    reference: Option<String>,
    tenant: u64,
) -> Result<CellId, Errno> {
    let _held = hold(daemon, &path).await?;
    // cladefs: what an import its host died in left, unmade, is no cell: made anew
    // (TASK-196).
    let checked = path.clone();
    let host = daemon.host.clone();
    blocking(move || {
        removed_if_unmade_on(&host, &checked)?;
        if host_fs::exists(&checked) {
            return Err(Errno::EEXIST);
        }
        Ok(())
    })
    .await?;
    // As the daemon's cells are kept: with the host's cache, which what the bucket
    // acknowledged of the image goes into and the import's check reads (TASK-237).
    // cladefs: an import under way, its path held: what its caller may give up on (board 5599);
    // on a blocking thread, as what a test has the failpoint do waits.
    blocking(|| {
        fail::fail_point!("cladefsd::import");
        Ok(())
    })
    .await?;
    // cladefs: awaited, its walk of a layer's entries async (board 5614).
    let imported = (image.as_path(), reference.as_deref());
    cladefs_fs::import::make_base_keeping_on(
        &daemon.host,
        &path,
        (capacity, tenant),
        imported,
        keeping(daemon)?,
    )
    .await
    .map_err(|err| errno_of(format_args!("importing {}", image.display()), err))?;
    let open_cell = open_here(daemon, &path).await?;
    let cell = open_cell.cell();
    open_cell.close().await;
    Ok(cell)
}

/// Makes a memory cell at `path` on `daemon`, which must not exist (`EEXIST`), holding the
/// snapshot memory file at `file`, optionally making an initial version (board 11222).
pub(super) async fn import_memory(
    daemon: &Arc<Daemon>,
    path: PathBuf,
    file: PathBuf,
    tenant: u64,
    make_version: bool,
    ephemeral: bool,
) -> Result<(CellId, Option<u64>), Errno> {
    if ephemeral && make_version {
        return Err(Errno::EINVAL);
    }
    let _held = hold(daemon, &path).await?;
    let checked = path.clone();
    let host = daemon.host.clone();
    blocking(move || {
        removed_if_unmade_on(&host, &checked)?;
        if host_fs::exists(&checked) {
            return Err(Errno::EEXIST);
        }
        Ok(())
    })
    .await?;
    let keeping = keeping(daemon)?;
    cladefs_fs::import::import_memory_keeping_on(
        &daemon.host,
        &path,
        &file,
        tenant,
        make_version,
        ephemeral,
        keeping,
    )
    .await
    .map_err(|err| errno_of(format_args!("importing memory {}", file.display()), err))
}

/// The cell at `path` on `daemon`, or, if it doesn't exist, one of `cell`, of `capacity` bytes,
/// taken from the daemon's bucket.
async fn take_cell(
    daemon: &Arc<Daemon>,
    path: &Path,
    cell: Option<CellId>,
    capacity: u64,
) -> Result<OpenCell, Errno> {
    let cell = match cell {
        Some(cell) => cell,
        None => local_cell::upload::blocking(|| {
            if !host_fs::exists(&path) {
                return Err(Errno::ENOENT);
            }
            local_cell::host_cell(path).map_err(|_| Errno::ENOENT)
        })?,
    };
    attached(daemon, path, async |keeping| {
        let opening = Opening::keeping_on(daemon.host.clone(), keeping).guest(false);
        OpenCell::take_with(path, (capacity, cell, None), opening).await
    })
    .await
}

/// Removes what a cell's making, or a fork's, left at `path` on `host`, unmade.
pub(super) fn removed_if_unmade_on(host: &cladefs_core::Host, path: &Path) -> Result<(), Errno> {
    if !cladefs_core::host_cell::unmade_on(host, path) {
        return Ok(());
    }
    local_cell::remove_cell_dir_on(host.local(), path).map_err(|err| {
        log::warn!("cladefsd: removing {}, a cell unmade: {err:#}", path.display());
        Errno::EIO
    })
}

/// cladefs: where `daemon` keeps the version `version` of `cell`'s cell open for the forks of it
/// made here ([`host_cell::version_dir`]). When no part cache is active, prefers the daemon's own
/// managed [`DiskPool`] root so multiple daemons in one process never collide on `/tmp`.
fn version_dir(daemon: &Daemon, cell: CellId, version: u64) -> Result<PathBuf, Errno> {
    if let Some(cache) = part_cache(daemon)? {
        return Ok(cladefs_core::host_cell::version_dir(cell, version, Some(&cache)));
    }
    if let Some(pool) = daemon.disk_pool()
        && let Some(disk) = pool.disks().first()
    {
        return Ok(disk
            .part_cache_dir
            .join("versions")
            .join(format!("{:x}.{:x}.{:x}", cell.tenant, cell.id, version)));
    }
    Ok(cladefs_core::host_cell::version_dir(cell, version, None))
}

/// cladefs: the directory the version `version` of `cell` is kept open in for forks here.
async fn opened_version(
    daemon: &Arc<Daemon>,
    cell: CellId,
    version: u64,
    capacity: u64,
) -> Result<PathBuf, Errno> {
    let capacity = capacity.max(OpenCell::BLOCK_SIZE as u64);
    let dir = version_dir(daemon, cell, version)?;
    // Fast path: if the version store is already kept open, its directory exists and is made
    // whole; return without taking a hold or closing it (TASK-549, board 2494).
    if kept(daemon, &cell_dir_key(&dir)).is_some() {
        return Ok(dir);
    }
    let held = hold_to_fork_within(daemon, &dir, VERSION_WAIT).await?;
    if local_cell::upload::blocking(|| {
        host_fs::exists(&dir) && !cladefs_core::host_cell::unmade_on(&daemon.host, &dir)
    }) {
        return Ok(dir);
    }
    close_kept(daemon, held.take_kept()).await;
    // Their paths alone: a reference to a cell kept across its sync would keep it from closing
    // (TASK-648).
    let writers: Vec<PathBuf> = (daemon.table.instances().into_iter())
        .filter_map(|(path, open_cell)| {
            inspect(daemon, open_cell, |open| (open.cell() == cell).then_some(path))
        })
        .collect();
    for path in writers {
        match sync(daemon, path.clone()).await {
            // Fenced: a later placement took the cell, on another host, recovering from what this
            // copy uploaded, the rest lost with it. What it uploaded is in the bucket, which the
            // version is read from, or found not named there (`ENOENT`): a version the cell's head
            // names forks on any host, its stale copy here or not.
            Err(Errno::EROFS) => log::info!(
                "cladefsd: {} fenced, a later placement's: version {version:x} read from the bucket",
                path.display()
            ),
            synced => synced?,
        }
    }
    // A cell its connection let go of is none of those, while it closes on a blocking thread,
    // uploading its last commits, the version's among them: its close waited for, if the version
    // was made here, before the bucket is read for it (TASK-644).
    if let Some(of) = cladefs_core::host_cell::version_made_of_on(&daemon.host, cell, version) {
        local_cell::closed_on(daemon.host.local(), &of).await.map_err(|err| {
            log::warn!("cladefsd: waiting for {} to close: {err}", of.display());
            Errno::EBUSY
        })?;
    }
    let bucket = daemon_bucket_on(daemon)?;
    let found = bucket_cell::head::get(&*bucket, cell).await;
    let object = found.map_err(|_| Errno::EIO)?.ok_or(Errno::ENOENT)?;
    let kind = object.kind.ok_or(Errno::ENOENT)?;
    let made = dir.clone();
    let host = daemon.host.clone();
    blocking(move || {
        let _held = held;
        removed_if_unmade_on(&host, &made)?;
        if let Some(root) = made.parent() {
            host_fs::create_dir_all(root).map_err(|_| Errno::EIO)?;
        }
        let url = cladefs_core::host_cell::bucket_for(&made).map_err(|_| Errno::EIO)?;
        let description =
            local_cell::Description { block_size: OpenCell::BLOCK_SIZE, capacity, cell };
        // What is public of the cell the version was made of, if made here, is inherited before
        // the version directory is marked made (board 5388), so a failed copy discards the
        // directory rather than leaving later restores/forks missing public markers.
        OpenCell::create_at_version_with_on(
            &host,
            &made,
            (&url, kind),
            description,
            version,
            || match cladefs_core::host_cell::version_made_of_on(&host, cell, version) {
                Some(of) => cladefs_core::cell_dir::inherit_public_on(host.local(), &of, &made)
                    .map_err(cladefs_core::Error::new),
                None => Ok(()),
            },
        )
        .map_err(|err| errno_of(format_args!("opening version {version} of {cell} here"), err))
    })
    .await?;
    Ok(dir)
}

/// Forks the version `version` of `cell` on `daemon`, a VM snapshot's, to a new cell at `path`.
pub(super) async fn fork_version(
    daemon: &Arc<Daemon>,
    (cell, version): (CellId, u64),
    capacity: u64,
    (path, tenant, named): (&Path, Option<u64>, Option<CellId>),
) -> Result<(), Errno> {
    let from = opened_version(daemon, cell, version, capacity).await?;
    match fork_kept(daemon, &from, capacity, (path, tenant, named)).await {
        (Err(errno), Opened::Before) if cell_at_fault(errno) => {
            // What a crash left at `path` goes first, as before any restore's.
            local_cell::upload::blocking(|| removed_if_unmade_on(&daemon.host, path))?;
            fork_kept(daemon, &from, capacity, (path, tenant, named)).await.0
        }
        (forked, _) => forked,
    }
}

/// cladefs: forks the version `version` of `cell` on `daemon` to a new cell at `to`.
pub(super) async fn fork_of_version(
    daemon: &Arc<Daemon>,
    cell: CellId,
    version: u64,
    to: PathBuf,
    tenant: Option<u64>,
) -> Result<CellId, Errno> {
    let _child = hold_to_fork(daemon, &to).await?;
    fork_version(daemon, (cell, version), OpenCell::BLOCK_SIZE as u64, (&to, tenant, None)).await?;
    local_cell::upload::blocking(|| local_cell::host_cell(&to)).map_err(|_| Errno::EIO)
}

/// Moves a per-cell upload-interval override from `from` to `to` after a staged cell directory is
/// renamed into its canonical `DiskPool` path.
pub(super) fn move_upload_interval(daemon: &Daemon, from: &Path, to: &Path) {
    let mut intervals =
        daemon.upload_intervals.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(interval) = intervals.remove(&cell_dir_key(from)) {
        intervals.insert(cell_dir_key(to), interval);
    }
}

/// cladefs: forks the version `version` of `cell` on `daemon` directly into
/// `parent_dir/<CHILD_PADDED>`, returning the new ephemeral cell ID and its final path.
pub(super) async fn fork_ephemeral_of_version_in(
    daemon: &Arc<Daemon>,
    cell: CellId,
    version: u64,
    parent_dir: PathBuf,
    tenant: Option<u64>,
) -> Result<(CellId, PathBuf), Errno> {
    let capacity = OpenCell::BLOCK_SIZE as u64;
    let from = opened_version(daemon, cell, version, capacity).await.inspect_err(|errno| {
        log::warn!("cladefsd: version {version:x} of {cell} for an ephemeral fork: {errno:?}")
    })?;
    let (kept, _) = kept_or_open(daemon, &from, capacity).await.inspect_err(|errno| {
        log::warn!("cladefsd: opening {} for an ephemeral fork: {errno:?}", from.display())
    })?;
    let tenant = tenant.unwrap_or(kept.open_cell.cell().tenant);
    let forked = kept.open_cell.fork_ephemeral_in(&parent_dir, tenant).await.map_err(|err| {
        errno_of(
            format_args!("forking {} ephemerally in {}", from.display(), parent_dir.display()),
            err,
        )
    });
    release_held(kept).await;
    forked
}

/// cladefs: forks the version `version` of `cell` on `daemon` to `to`, an ephemeral cell kept on
/// this host alone (doc-25, TASK-698.3; `ForkCell`'s `ephemeral`).
#[cfg(test)]
pub(super) async fn fork_ephemeral_of_version(
    daemon: &Arc<Daemon>,
    cell: CellId,
    version: u64,
    to: PathBuf,
    tenant: Option<u64>,
) -> Result<CellId, Errno> {
    let _child = hold_to_fork(daemon, &to).await?;
    let capacity = OpenCell::BLOCK_SIZE as u64;
    let from = opened_version(daemon, cell, version, capacity).await.inspect_err(|errno| {
        log::warn!("cladefsd: version {version:x} of {cell} for an ephemeral fork: {errno:?}")
    })?;
    let (kept, _) = kept_or_open(daemon, &from, capacity).await.inspect_err(|errno| {
        log::warn!("cladefsd: opening {} for an ephemeral fork: {errno:?}", from.display())
    })?;
    let tenant = tenant.unwrap_or(kept.open_cell.cell().tenant);
    let forked = kept.open_cell.fork_ephemeral(&to, tenant).await.map_err(|err| {
        errno_of(format_args!("forking {} ephemerally to {}", from.display(), to.display()), err)
    });
    release_held(kept).await;
    forked
}

/// cladefs: `Ok` if `open_cell`, at a restore's `path` already on `daemon`, holds what the version
/// `version` of `cell` does ([`OpenCell::holds_what`]).
pub(super) async fn at_version<S>(
    daemon: &Arc<Daemon>,
    (cell, version): (CellId, u64),
    capacity: u64,
    path: &Path,
    open_cell: &Arc<S>,
) -> Result<(), Errno>
where
    S: Borrow<OpenCell> + Send + Sync + 'static,
{
    // The snapshotted cell itself: its commits uploaded first, the version's among them, as a
    // version is local as it is made and uploaded behind.
    if (**open_cell).borrow().cell() == cell {
        uploaded(open_cell).await?;
    }
    let from = opened_version(daemon, cell, version, capacity).await?;
    let (kept, _) = kept_or_open(daemon, &from, capacity).await?;
    let holds = (**open_cell).borrow().holds_what(&kept.open_cell);
    release_held(kept).await;
    if holds? {
        return Ok(());
    }
    log::warn!(
        "cladefsd: refusing a restore of version {version} of {cell} into {}: written since, it \
         holds another state than the snapshot's",
        path.display()
    );
    Err(Errno::EEXIST)
}

/// cladefs: forks the kept cell at `from` to `path` on `daemon`, once.
async fn fork_kept(
    daemon: &Arc<Daemon>,
    from: &Path,
    capacity: u64,
    (path, tenant, named): (&Path, Option<u64>, Option<CellId>),
) -> (Result<(), Errno>, Opened) {
    let (kept, opened) = match kept_or_open(daemon, from, capacity).await {
        Ok(kept) => kept,
        Err(errno) => return (Err(errno), Opened::Now),
    };
    // A new cell of `tenant`'s, or of the kept cell's tenant's (TASK-364).
    let tenant = tenant.unwrap_or(kept.open_cell.cell().tenant);
    let failed = Arc::downgrade(&kept);
    // cladefs: awaited, its disk work off the runtime's threads (TASK-685.3).
    let forked = match named {
        Some(cell) => kept.open_cell.fork_into(path, cell).await,
        None => kept.open_cell.fork(path, tenant).await.map(drop),
    }
    .map_err(|err| errno_of(format_args!("forking {} to {}", from.display(), path.display()), err));
    release_held(kept).await;
    if forked.is_err_and(cell_at_fault) {
        let_go_failed(daemon, &cell_dir_key(from), failed).await;
    }
    (forked, opened)
}

/// cladefs: whether a fork that failed with `errno` may have failed by the cell it forks.
fn cell_at_fault(errno: Errno) -> bool {
    !matches!(errno, Errno::EEXIST | Errno::EACCES)
}

/// cladefs: when a restore's kept cell was opened ([`kept_or_open`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Opened {
    /// For it, or for the restores beside it.
    Now,
    /// For a restore before it, kept open since ([`keep`]).
    Before,
}

/// cladefs: the kept cell at `from` on `daemon` for a restore to fork.
async fn kept_or_open(
    daemon: &Arc<Daemon>,
    from: &Path,
    capacity: u64,
) -> Result<(Arc<HeldCell>, Opened), Errno> {
    if let Some(kept) = kept(daemon, &cell_dir_key(from)) {
        return Ok((kept, Opened::Before));
    }
    // TASK-443.6: however long a restore beside it takes to open it ([`VERSION_WAIT`]).
    let hold = daemon.table.hold_async(&cell_dir_key(from), VERSION_WAIT).await?;
    // Opened by a restore that held it before, beside this one.
    if let Some(kept) = kept(daemon, hold.key()) {
        return Ok((kept, Opened::Now));
    }
    if let Some(open_cell) = hold.instance() {
        return Ok((Arc::new(HeldCell::new(open_cell, Some(hold))), Opened::Now));
    }
    // cladefs: awaited, holding no worker of the control runtime's (TASK-688).
    #[cfg(test)]
    daemon.kept_opens.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok((keep(daemon, &hold, take_cell(daemon, from, None, capacity).await?), Opened::Now))
}

/// Forks the cell at `from` to a new cell at `to` on `daemon`, and returns its cell.
pub(super) async fn fork(
    daemon: &Arc<Daemon>,
    from: PathBuf,
    to: PathBuf,
    tenant: Option<u64>,
) -> Result<CellId, Errno> {
    fork_from_cell(daemon, &from, &to, tenant).await
}

/// cladefs: forks the cell at `from` to `to` on `daemon` as [`fork`] does.
async fn fork_from_cell(
    daemon: &Arc<Daemon>,
    from: &Path,
    to: &Path,
    tenant: Option<u64>,
) -> Result<CellId, Errno> {
    let _child = hold_to_fork(daemon, to).await?;
    let key = cell_dir_key(from);
    // A new cell of `tenant`'s, or of the cell's tenant's (TASK-364).
    let from_cell =
        local_cell::upload::blocking(|| local_cell::host_cell(from)).map_err(|_| Errno::ENOENT)?;
    let tenant = tenant.unwrap_or(from_cell.tenant);
    let was_kept = kept(daemon, &key).is_some();
    match fork_held(daemon, from, to, tenant, &key).await {
        Err(errno) if was_kept && cell_at_fault(errno) => {
            fork_held(daemon, from, to, tenant, &key).await
        }
        forked => forked,
    }
}

/// cladefs: forks the cell at `from` on `daemon`, whose key is `key`, to `to`.
async fn fork_held(
    daemon: &Arc<Daemon>,
    from: &Path,
    to: &Path,
    tenant: u64,
    key: &Path,
) -> Result<CellId, Errno> {
    let forking = match kept(daemon, key) {
        Some(kept) => Forked { here: Here::kept(daemon, kept), live: false },
        None => {
            daemon
                .table
                .fork_cell(
                    key,
                    CLOSING_WAIT,
                    |connection| async move {
                        let open_cell = connection.cell().ok_or(Errno::EBUSY)?.clone();
                        Ok(Forked { here: Here::of(daemon, open_cell, None), live: true })
                    },
                    |hold| async move {
                        if let Some(open_cell) = daemon.table.instance(key) {
                            Ok(Forked { here: Here::of(daemon, open_cell, hold), live: true })
                        } else if let Some(kept) = kept(daemon, key) {
                            // Opened by a restore that held it before.
                            Ok(Forked { here: Here::kept(daemon, kept), live: false })
                        } else {
                            let open_cell = open_here(daemon, from).await?;
                            // Hold let go of once kept.
                            let here = match hold {
                                Some(hold) => Here::kept(daemon, keep(daemon, &hold, open_cell)),
                                None => Here::of(daemon, Arc::new(open_cell), None),
                            };
                            Ok(Forked { here, live: false })
                        }
                    },
                )
                .await?
        }
    };
    let (held, live) = (forking.here.held().clone(), forking.live);
    let weak = Arc::downgrade(&held);
    let to = to.to_owned();
    let forked = async {
        uploaded(&held).await?;
        fail::fail_point!("cladefsd::fork_uploaded", |_| Err(Errno::EIO));
        let forked = match live {
            true => held.open_cell.fork_quiescent(&to, tenant).await,
            false => held.open_cell.fork(&to, tenant).await,
        };
        let cell =
            forked.map_err(|err| errno_of(format_args!("forking to {}", to.display()), err))?;
        // cladefs: the fork made, its reply lost: its caller takes it to have failed.
        fail::fail_point!("cladefsd::forked", |_| Err(Errno::EIO));
        Ok(cell)
    }
    .await;
    release_held(held).await;
    forking.close().await;
    if forked.as_ref().is_err_and(|errno| cell_at_fault(*errno)) {
        let_go_failed(daemon, key, weak).await;
    }
    forked
}

/// cladefs: awaits `work` with the cell at `path` on `daemon`.
async fn with_cell<T>(
    daemon: &Arc<Daemon>,
    path: &Path,
    work: impl AsyncFnOnce(&OpenCell) -> Result<T, Errno>,
) -> Result<T, Errno> {
    if let Some(open_cell) = open_by_connection(daemon, path) {
        let done = work(&open_cell).await;
        release(open_cell).await;
        return done;
    }
    let held = hold(daemon, path).await?;
    // A cell its connection let go of may live on a while, its last reference dropped on a
    // blocking thread: worked on as it is, under the hold, never opened a second time beside it,
    // as sync does (board 2730, board 2922).
    if let Some(open_cell) = held.instance() {
        let done = work(&open_cell).await;
        // Closed here, if this was its last reference, holding no thread.
        release(open_cell).await;
        return done;
    }
    let here = Here::open(daemon, path, held).await?;
    let done = work(&here.held().open_cell).await;
    here.close().await;
    done
}

/// cladefs: makes a version of the cell at `path` on `daemon` ([`OpenCell::make_version`]).
pub(super) async fn make_version(
    daemon: &Arc<Daemon>,
    path: PathBuf,
    keep_last: Option<u32>,
) -> Result<u64, Errno> {
    with_cell(daemon, &path, async |open_cell| {
        let made = open_cell.make_version(keep_last).await;
        made.map_err(|err| errno_of(format_args!("making a version of {}", path.display()), err))
    })
    .await
}

/// cladefs: the versions of the cell at `path` on `daemon` ([`OpenCell::versions`]).
pub(super) async fn versions(
    daemon: &Arc<Daemon>,
    path: PathBuf,
) -> Result<Vec<cladefs_core::VersionInfo>, Errno> {
    with_cell(daemon, &path, async |open_cell| Ok(open_cell.versions().await)).await
}

/// cladefs: drops the versions `ids` of the cell at `path` on `daemon`.
pub(super) async fn drop_versions(
    daemon: &Arc<Daemon>,
    path: PathBuf,
    ids: BTreeSet<u64>,
) -> Result<(), Errno> {
    with_cell(daemon, &path, async |open_cell| {
        let dropped = open_cell.drop_versions(&ids).await;
        dropped
            .map_err(|err| errno_of(format_args!("dropping versions of {}", path.display()), err))
    })
    .await
}

/// cladefs: shares the version `id` of the cell at `path` on `daemon`, or no longer.
pub(super) async fn share_version(
    daemon: &Arc<Daemon>,
    path: PathBuf,
    id: u64,
    shared: bool,
) -> Result<(), Errno> {
    with_cell(daemon, &path, async |open_cell| {
        let shared = open_cell.share_version(id, shared).await;
        shared.map_err(|err| {
            errno_of(format_args!("sharing version {id} of {}", path.display()), err)
        })
    })
    .await
}

/// Waits until every commit of the cell at `path` on `daemon` is in the daemon's bucket.
pub(super) async fn sync(daemon: &Arc<Daemon>, path: PathBuf) -> Result<(), Errno> {
    if let Some(open_cell) = open_by_connection(daemon, &path) {
        let synced = uploaded(&open_cell).await;
        release(open_cell).await;
        return synced;
    }
    let held = hold(daemon, &path).await?;
    // A cell its connection let go of may live on a while, its last reference dropped on a
    // blocking thread: synced as it is, under the hold, never opened a second time beside it,
    // which this process's claim on its directory refuses (TASK-648).
    if let Some(open_cell) = held.instance() {
        let synced = uploaded(&open_cell).await;
        release(open_cell).await;
        return synced;
    }
    let here = Here::open(daemon, &path, held).await?;
    let synced = uploaded(here.held()).await;
    here.close().await;
    synced
}

/// Deletes the cell at `path` on `daemon`, or the fork whose handle it is, and its branch in its
/// bucket ([`OpenCell::delete`], [`bucket_of`]): `EBUSY` while a connection has it open.
pub(super) async fn delete(daemon: &Arc<Daemon>, path: PathBuf) -> Result<(), Errno> {
    let bucket = local_cell::upload::blocking(|| bucket_of(daemon, &path))?;
    delete_alone(daemon, path, Some(bucket)).await
}

/// The bucket the cell at `path`, or the fork whose handle it is, keeps its branch in.
fn bucket_of(
    daemon: &Arc<Daemon>,
    path: &Path,
) -> Result<Arc<bucket_cell::object_bucket::RoutedBucket>, Errno> {
    match cladefs_core::host_cell::bucket_at_on(&daemon.host, path) {
        Ok(Some(bucket)) => Ok(bucket),
        Ok(None) => daemon_bucket_on(daemon),
        Err(err) => {
            log::warn!("cladefsd: the bucket of {}: {err:#}", path.display());
            Err(Errno::EIO)
        }
    }
}

/// cladefs: deletes this host's kept cell at `path` on `daemon`, leaving its branch in the bucket.
pub(super) async fn drop_here(daemon: &Arc<Daemon>, path: PathBuf) -> Result<(), Errno> {
    delete_alone(daemon, path, None).await
}

/// Deletes the cell at `path` on `daemon`, and its branch in `bucket`, if given.
async fn delete_alone(
    daemon: &Arc<Daemon>,
    path: PathBuf,
    bucket: Option<Arc<bucket_cell::object_bucket::RoutedBucket>>,
) -> Result<(), Errno> {
    let _held = hold_alone(daemon, &path).await?;
    let key = cell_dir_key(&path);
    // Refuse deleting a cell with an active outgoing migration (preparing, moving, or handing
    // over) and prevent any new MigrateCell from starting until removal completes (board 5376).
    let _deleting = crate::migration::guard_delete(daemon, &key)?;
    // Refuse deleting a cell given up to a live migration so a bucket cell is never deleted mid
    // hand-over (board 11530).
    let check = path.clone();
    if blocking(move || Ok(host_fs::exists(check.join(crate::migration::MARKER)))).await? {
        return Err(Errno::EBUSY);
    }
    let deleted = OpenCell::delete_on(&daemon.host, &path, bucket.as_deref()).await;
    if deleted.is_ok() {
        daemon
            .upload_intervals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&key);
    }
    deleted.map_err(|err| errno_of(format_args!("deleting {}", path.display()), err))
}

/// Sets the capacity of the cell at `path` on `daemon` to `capacity` bytes.
pub(super) async fn resize(
    daemon: &Arc<Daemon>,
    path: PathBuf,
    new_capacity: u64,
) -> Result<(), Errno> {
    let key = cell_dir_key(&path);
    daemon
        .table
        .resize_cell(
            &key,
            new_capacity,
            |writer, cap| async move { writer.resize(cap).await },
            |hold| {
                let key = key.clone();
                async move {
                    let open_cell = match daemon.table.instance(&key) {
                        Some(open_cell) => open_cell,
                        None => {
                            if let Some(hold) = &hold {
                                close_kept(daemon, hold.take_kept()).await;
                            }
                            if !local_cell::upload::blocking(|| host_fs::exists(&key)) {
                                return Err(Errno::ENOENT);
                            }
                            Arc::new(open_here(daemon, &key).await?)
                        }
                    };
                    Ok(Here::of(daemon, open_cell, hold))
                }
            },
            |here| capacity(&here.held().open_cell),
            async |here, cap| set_capacity(here.held(), cap).await,
            // Closed before those waiting to open it may (it once let them wait out CLOSING_WAIT:
            // found by the cell table's Shuttle tests, TASK-44.6).
            |here| here.close(),
        )
        .await
}

/// Places the cell at `path` on `daemon` at the placement whose fencing token is `token`.
pub(super) async fn place(daemon: &Arc<Daemon>, path: PathBuf, token: u64) -> Result<(), Errno> {
    let held = hold_alone(daemon, &path).await?;
    let host = daemon.host.clone();
    blocking(move || {
        let _held = held;
        if host_fs::exists(path.join(crate::migration::MARKER)) {
            return Err(Errno::EBUSY);
        }
        OpenCell::place_on(&host, &path, token)
            .map_err(|err| errno_of(format_args!("placing {}", path.display()), err))
    })
    .await
}

/// Makes the cell at `path` on `daemon` a public base.
pub(super) async fn make_public(daemon: &Arc<Daemon>, path: PathBuf) -> Result<(), Errno> {
    let _held = hold_alone(daemon, &path).await?;
    let open_cell = Arc::new(open_here(daemon, &path).await?);
    let making = open_cell.clone();
    let made = blocking(move || making.make_public()).await;
    if let Some(open_cell) = Arc::into_inner(open_cell) {
        open_cell.close().await;
    }
    made
}

/// What the cell at `path` is, and holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CellState {
    pub cell: CellId,
    pub info: ExtentInfo,
    /// What its cell holds, bytes and extents, and whose (TASK-49, TASK-454).
    pub holding: cladefs_core::CellHolding,
    /// Whether it is a public base.
    pub public: bool,
    /// Whether a connection has it open.
    pub open: bool,
    /// The epoch of its latest commit.
    pub committed: u64,
    /// What its uploader has done and has left to do, if its commits upload now.
    pub upload: Option<local_cell::upload::UploadStatus>,
    /// Whether this cell is a host-local ephemeral cell (doc-25).
    pub ephemeral: bool,
    /// The fencing token of this host's placement of it, open or not ([`placement_token`]): none
    /// for an ephemeral cell, which no bucket keeps.
    pub placement: Option<u64>,
}

/// What `open_cell`, the cell at `path`, is, and holds; `open` if a connection has it open.
fn state_of(open_cell: &OpenCell, path: &Path, open: bool) -> Result<CellState, Errno> {
    let ephemeral = open_cell.is_ephemeral();
    Ok(CellState {
        cell: open_cell.cell(),
        info: open_cell.info()?,
        holding: open_cell.holding()?,
        public: open_cell.is_public(),
        open,
        committed: open_cell.committed()?,
        upload: open_cell.upload_status()?,
        ephemeral,
        placement: (!ephemeral).then(|| placement_token(path)).transpose()?,
    })
}

/// The fencing token of this host's placement of the bucket-backed cell at `path`, whether a
/// connection has it open or not (board 13879): the one it was placed at, as its description
/// says ([`place`]); or, never placed, the daemon's own, which its uploader takes it with
/// ([`keeping`]).
fn placement_token(path: &Path) -> Result<u64, Errno> {
    let placed = local_cell::upload::blocking(|| local_cell::host_token(path)).map_err(|err| {
        log::warn!("cladefsd: reading the placement of {}: {err:#}", path.display());
        Errno::EIO
    })?;
    placed.map_or_else(daemon_token, Ok)
}

/// The fencing token of this host's placement (`CLADEFSD_TOKEN`): `EINVAL` if it names none,
/// logged.
fn daemon_token() -> Result<u64, Errno> {
    cladefs_core::bucket::host_token().map_err(|err| {
        log::error!("cladefsd: {err}");
        Errno::EINVAL
    })
}

/// Reads an open cell's metadata. If its owner let it go meanwhile, close it in a task of its
/// own, like a connection's teardown does (board 10569).
pub(super) fn inspect<T>(
    daemon: &Daemon,
    cell: Arc<OpenCell>,
    read: impl FnOnce(&OpenCell) -> T,
) -> T {
    let result = read(&cell);
    if let Some(cell) = Arc::into_inner(cell) {
        cell.mark_closing();
        drop(daemon.spawn(cell.close()));
    }
    result
}

/// The cells open on `daemon`, writers' and others', by their paths (TASK-50.5).
pub(super) fn list(daemon: &Daemon) -> Result<Vec<(PathBuf, CellState)>, Errno> {
    let mut listed = Ok(Vec::new());
    for (path, cell) in daemon.table.instances() {
        // Even after an error, release every cell through inspect.
        inspect(daemon, cell, |cell| {
            if let Ok(cells) = &mut listed {
                match state_of(cell, &path, true) {
                    Ok(state) => cells.push((path, state)),
                    Err(err) => listed = Err(err),
                }
            }
        });
    }
    listed
}

/// What the cell at `path` on `daemon` is, and holds.
pub(super) async fn stat(daemon: &Arc<Daemon>, path: PathBuf) -> Result<CellState, Errno> {
    if let Some(open_cell) = open_by_connection(daemon, &path) {
        return inspect(daemon, open_cell, |cell| state_of(cell, &path, true));
    }
    let _held = hold(daemon, &path).await?;
    if !local_cell::upload::blocking(|| host_fs::exists(&path)) {
        return Err(Errno::ENOENT);
    }
    let open_cell = open_here(daemon, &path).await?;
    let state = state_of(&open_cell, &path, false);
    open_cell.close().await;
    state
}

/// How many cells connections have open on `daemon`, to write (`metrics`).
pub(super) fn open_count(daemon: &Daemon) -> usize {
    daemon.table.writers()
}

/// The cells open for connections on `daemon` whose commits upload now, and what their uploaders
/// have left to do (`metrics`, TASK-91.3).
pub(super) fn upload_statuses(daemon: &Daemon) -> Vec<(CellId, local_cell::upload::UploadStatus)> {
    (daemon.table.instances().into_iter())
        .filter_map(|(_, cell)| {
            inspect(daemon, cell, |cell| {
                // An uploader that stopped on error has no live status to show.
                Some((cell.cell(), cell.upload_status().ok()??))
            })
        })
        .collect()
}

/// Convenience wrappers for non-madsim unit tests operating on `Daemon::process()`.
#[cfg(all(test, not(madsim)))]
pub(super) fn table() -> &'static Table {
    &Daemon::process().table
}

#[cfg(all(any(test, feature = "testing"), not(madsim)))]
pub fn daemon_bucket() -> Result<Arc<bucket_cell::object_bucket::RoutedBucket>, Errno> {
    daemon_bucket_on(Daemon::process())
}

#[cfg(all(test, not(madsim)))]
pub(super) fn use_bucket(bucket: impl Into<Arc<bucket_cell::object_bucket::RoutedBucket>>) {
    crate::storage::use_bucket_on(Daemon::process(), bucket);
}

#[cfg(all(test, not(madsim)))]
pub(super) use crate::storage::uploads_when_asked;
