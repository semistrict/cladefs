// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The daemon's stores, by the paths that name them: those VM processes' connections have open,
//! and what the daemon does to a store by its path, for a connection or the control API
//! ([`super::control`]), holding it meanwhile, so that no connection opens it then.
//!
//! What the control API asks for is a future (TASK-43.6): its waits, for uploads, a guest's
//! answer, and a connection's letting a store go, hold no thread, and its stores' I/O runs on the
//! runtime's blocking threads ([`blocking`]), as do opening and closing a store, which waits for
//! another process's lock of it, and its cell's take from object storage.
//!
//! Who holds a store, and what is open of it, is the daemon's one table's (`crate::cell_table`,
//! TASK-160.8), each hold a value that lets the store go as it is dropped. cladefs: a store open for
//! connections is one instance ([`OpenCell`]), its writer's, if a connection writes it, and its
//! followers', which follow it (backlog doc-8). Followers that find no writer open it as a writer
//! would, and a writer opening it then takes it over ([`OpenCell::take_over`]), as it does a
//! writer's that went while followers followed it, if nothing is open in it. A writer that went
//! with writes uncommitted retires its store: its followers are cut off, and neither a writer nor
//! a follower opens it until they are gone.

use super::server::Connection;
use crate::cell_requests::{self, Capacity, Close};
use crate::cell_table::{self, CellTable, Writer as _};
use crate::open_cell::{Opening, release};
use crate::pages::Arena;
use crate::{CellId, CellOf, Errno, ExtentInfo, OpenCell};
use bucket_cell::cache::Cache;
use bucket_cell::cell::Kind;
use bucket_cell::object_bucket::Stats;
use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

/// The daemon's stores, by their keys ([`store_key`]): who holds each, a connection writing it or
/// the daemon working on it, and what is open of it, the store connections have open or a kept store
/// kept open for forks.
pub(super) type Table = CellTable<Connection, OpenCell, Kept>;

/// A store held ([`table`]): by a connection writing it, or by the daemon working on it, which no
/// connection opens meanwhile; until it is dropped.
pub(super) type Hold = cell_table::Hold<'static, Connection, OpenCell, Kept>;

/// The daemon's stores ([`Table`]).
pub(super) fn table() -> &'static Table {
    node_local::node_local! {
        static TABLE: OnceLock<Table> = OnceLock::new();
    }
    TABLE.get().get_or_init(CellTable::new)
}

/// Holds the store `key` names for `connection` to write it, once a connection whose VM process
/// is gone lets it go, waiting at most [`CLOSING_WAIT`] (`EBUSY`), holding no thread meanwhile:
/// `EBUSY` while a connection not going writes it. cladefs: the store kept open of it for forks,
/// which the hold takes, closed, once the forks of it under way are done.
pub(super) async fn hold_to_write(key: &Path, connection: Weak<Connection>) -> Result<Hold, Errno> {
    let (hold, kept) = table().hold_to_write(key, connection, CLOSING_WAIT).await?;
    close_kept(kept).await;
    Ok(hold)
}

/// cladefs: the store followers have open at the key `hold` holds for a writer, or a writer's that
/// went, which the writer takes over ([`OpenCell::take_over`]), if one is: `EBUSY` if it is
/// open in an arena not `arena` ([`in_arena`]), retired, or something is open in it; and as the
/// store refuses a writer, a kept store or a public base ([`crate::CellAccess::may_write`]).
pub(super) fn take_over(hold: &Hold, arena: &Arc<Arena>) -> Result<Option<Arc<OpenCell>>, Errno> {
    let Some((store, retired)) = hold.instance() else { return Ok(None) };
    let key = hold.key();
    if let Err(errno) = in_arena(&store, arena) {
        log::warn!("cladefsd: {} is open for followers in another arena: {errno}", key.display());
        return Err(errno);
    }
    match if retired { Err(Errno::EBUSY) } else { store.take_over() } {
        Ok(()) => Ok(Some(store)),
        Err(Errno::EBUSY) => {
            log::warn!(
                "cladefsd: {} is open for followers, which its writer that went left writes \
                 uncommitted in",
                key.display()
            );
            Err(Errno::EBUSY)
        }
        Err(errno) => Err(errno),
    }
}

/// cladefs: retires `store`, which the connection holding it as `hold` wrote, gone with writes
/// uncommitted: marked so, and then its followers cut off ([`Hold::retire`]), and no connection
/// opens it until they are gone.
pub(super) fn retire(hold: &Hold, store: &OpenCell) {
    hold.retire(|| store.cut_followers());
}

/// cladefs: a follower of the store at `path` (backlog doc-8): of the one a writer's connection, or
/// followers', have open ([`crate::cell_table::CellTable::follow`]); or, if none has, of the
/// store opened as of its last commit, as a writer opens it, its pages of `arena`, which a writer
/// opening it then takes over. `EBUSY` while a writer is opening it, or it is retired, or as it is
/// followed, or open in another arena ([`in_arena`]).
pub(super) async fn followed(path: &Path, arena: &Arc<Arena>) -> Result<OpenCell, Errno> {
    let follow = |store: &Arc<OpenCell>| in_arena(store, arena).map(|()| store.follow());
    let key = store_key(path);
    if let Some(follower) = table().follow(&key, follow)? {
        return Ok(follower);
    }
    // No other opens it meanwhile: a follower, a writer, or the daemon's work; nor retires it,
    // which only its writer does.
    let hold = hold(&key).await?;
    match hold.instance() {
        Some((_, true)) => return Err(Errno::EBUSY),
        Some((store, false)) => return follow(&store),
        None => {}
    }
    if !host_fs::exists(&path) {
        return Err(Errno::ENOENT);
    }
    let store = Arc::new(open_store(path, None, Some(arena)).await?);
    hold.open(&store)?;
    Ok(store.follow())
}

/// cladefs: `EBUSY` unless `store`'s own pages are of `arena`: the connection's, which its VM
/// process holds (TASK-79.36). Its window maps no other arena's pages, and the default's read-only
/// alone, its tenant's being its own.
fn in_arena(store: &OpenCell, arena: &Arc<Arena>) -> Result<(), Errno> {
    if store.arena() == arena.id() { Ok(()) } else { Err(Errno::EBUSY) }
}

/// How long holding a store waits for a connection whose VM process is gone to let it go: as a VM
/// restored from a snapshot at once opens the store its VM had (TASK-39.1).
pub(super) const CLOSING_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// cladefs: how long holding a version's store waits for its holder (TASK-443.6): as long as it
/// holds it. Its holders are the daemon's own work, which ends: a restore opening it from the
/// bucket for the restores beside it, which waited [`CLOSING_WAIT`] at most before and failed
/// `EBUSY` (11 of 20 restores of one version, its opening taking 10.9 s from a bucket 600 ms
/// away); a sync of it; its close, once no restore forked it for a while.
const VERSION_WAIT: std::time::Duration = std::time::Duration::MAX;

/// The path that names the store at `path` in the daemon's [`table`]: whichever way a request
/// names it.
pub(super) fn store_key(path: &Path) -> PathBuf {
    if let Ok(path) = host_fs::canonicalize(&path) {
        return path;
    }
    // A store not yet made: where it will be.
    match (path.parent().map(host_fs::canonicalize), path.file_name()) {
        (Some(Ok(parent)), Some(name)) => parent.join(name),
        _ => path.to_owned(),
    }
}

/// cladefs: holds the store at `path` for the daemon's FUSE mount of cells, which attaches it
/// (TASK-475.3): until what it returns is dropped, as the mount detaches it, a VM process opening
/// it, or the control API working on it, fails with `EBUSY` at once. `EBUSY` if a connection not
/// going writes it, or followers have it open; waits for one going, or the daemon's work, at most
/// [`CLOSING_WAIT`]. The store kept open of it for forks, if any, closed. It blocks.
pub(crate) fn hold_mounted(path: &Path) -> Result<MountHold, Errno> {
    let (hold, kept) = table().hold_mounted(&store_key(path), CLOSING_WAIT)?;
    close_kept_blocking(kept);
    Ok(MountHold(hold))
}

/// cladefs: a store the daemon's FUSE mount holds ([`hold_mounted`]), let go as it is dropped.
pub(crate) struct MountHold(#[allow(dead_code, reason = "held for its drop")] Hold);

/// Holds the store at `path` for the daemon, which no connection opens meanwhile: `EBUSY` while a
/// connection writes it, once a connection whose VM process is gone lets it go, waiting at most
/// [`CLOSING_WAIT`], holding no thread. cladefs: the store kept open of it for forks, if any,
/// closed: the daemon's work on it but forking it has it alone (TASK-92.5).
async fn hold(path: &Path) -> Result<Hold, Errno> {
    hold_within(path, CLOSING_WAIT).await
}

/// cladefs: holds the store at `path` for the daemon, as [`hold`] does, waiting at most `wait`.
async fn hold_within(path: &Path, wait: std::time::Duration) -> Result<Hold, Errno> {
    let hold = table().hold_async(&store_key(path), wait).await?;
    close_kept(hold.take_kept()).await;
    Ok(hold)
}

/// cladefs: holds the store at `path` for the daemon, as [`hold`] does, but for the store kept open
/// of it, which it leaves open: to fork it, or open it to ([`kept_or_open`]).
async fn hold_to_fork(path: &Path) -> Result<Hold, Errno> {
    table().hold_async(&store_key(path), CLOSING_WAIT).await
}

/// Holds the store at `path` for the daemon, as [`hold`] does: `EBUSY` while followers have it
/// open too.
/// cladefs: holds the store at `path`, which no connection has open (`EBUSY` otherwise), as a live
/// migration's hand-over does (TASK-698.4).
pub(super) async fn hold_store_alone(path: &Path) -> Result<Hold, Errno> {
    hold_alone(path).await
}

async fn hold_alone(path: &Path) -> Result<Hold, Errno> {
    let hold = hold(path).await?;
    if hold.instance().is_some() {
        return Err(Errno::EBUSY);
    }
    Ok(hold)
}

/// Runs `work`, which blocks, on one of the runtime's blocking threads, and returns what it
/// returns.
pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Errno> + Send + 'static,
) -> Result<T, Errno> {
    super::run::off_the_runtime(work).await.unwrap_or_else(|err| {
        log::error!("cladefsd: a store's blocking work: {err}");
        Err(Errno::EIO)
    })
}

/// A store the daemon works on, and what it lets go of once the store is closed, if anything,
/// dropped in that order (TASK-79.34): the daemon's hold of its path is let go only once the
/// store, one the daemon opened, is closed, whoever drops it last, a call or a blocking thread
/// whose work a cancelled call left going.
pub(super) struct HeldCell {
    store: Arc<OpenCell>,
    _then: Option<Then>,
}

/// What a store the daemon works on lets go of once it is closed ([`HeldCell`]).
enum Then {
    /// The daemon's hold of its path.
    LetGo { _hold: Hold },
    /// cladefs: what says a store kept open is closed, to what closes it ([`Kept`]).
    SayClosed { _closing: futures::channel::oneshot::Sender<std::convert::Infallible> },
}

impl Borrow<OpenCell> for HeldCell {
    fn borrow(&self) -> &OpenCell {
        &self.store
    }
}

/// A store the daemon works on ([`HeldCell`]): one it opened, or one open for connections, which
/// it lets go of in a task of its own ([`release_held`]), so that a call cancelled as it lets it go
/// leaves it to close all the same.
struct Here(Option<Arc<HeldCell>>);

impl Here {
    /// The store at `path`, opened on a blocking thread ([`open_here`]), which `hold` holds.
    async fn open(path: &Path, hold: Hold) -> Result<Self, Errno> {
        let store = Arc::new(open_here(path).await?);
        Ok(Self(Some(Arc::new(HeldCell::new(store, Some(hold))))))
    }

    /// `store`, which `hold` holds, if anything does: one open for connections, or opened here.
    fn of(store: Arc<OpenCell>, hold: Option<Hold>) -> Self {
        Self(Some(Arc::new(HeldCell::new(store, hold))))
    }

    /// cladefs: `kept`, a store kept open for forks ([`keep`]).
    fn kept(kept: Arc<HeldCell>) -> Self {
        Self(Some(kept))
    }

    fn store(&self) -> &Arc<HeldCell> {
        self.0.as_ref().expect("an open store")
    }
}

impl HeldCell {
    /// `store`, which `hold` holds, if anything does.
    fn new(store: Arc<OpenCell>, hold: Option<Hold>) -> Self {
        Self { store, _then: hold.map(|_hold| Then::LetGo { _hold }) }
    }

    /// cladefs: lets it go: its store, closed if this was its last reference ([`release`]), and
    /// then what it lets go of once that is closed, in the order its drop would (TASK-79.34), holding
    /// no thread (doc-24, step 9).
    async fn close(self) {
        let HeldCell { store, _then } = self;
        release(store).await;
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

/// cladefs: lets `held` go in a task of its own, on the runtime that lives as long as the daemon:
/// what lets one go where it can't await its close, as a drop, or a call that may be cancelled.
fn release_held_behind(held: Arc<HeldCell>) -> tokio::task::JoinHandle<()> {
    super::server::IO.spawn(release_held(held))
}

impl Drop for Here {
    fn drop(&mut self) {
        if let Some(store) = self.0.take() {
            drop(release_held_behind(store));
        }
    }
}

impl Close for Here {
    async fn close(mut self) {
        if let Some(store) = self.0.take() {
            if let Err(err) = release_held_behind(store).await {
                log::error!("cladefsd: closing a store: {err}");
            }
        }
    }
}

impl Capacity for Here {
    fn capacity(&self) -> Result<(u64, u64), Errno> {
        capacity(&self.store().store)
    }

    async fn set_capacity(&self, capacity: u64) -> Result<(), Errno> {
        set_capacity(self.store(), capacity).await
    }
}

/// cladefs: how long a kept store the daemon opened to fork stays open with no fork of it (TASK-92.5):
/// a zygote's restores, which fork its snapshot's kept store, come together, and the kept store's opening,
/// attaching and closing, a second or more with object storage a region away, is paid once for
/// them, rather than by each in turn, the last of them waiting for all those before it.
const LINGER: std::time::Duration = std::time::Duration::from_secs(60);

/// cladefs: a kept store the daemon opened to fork, kept open for the forks after it ([`keep`]): its
/// store, which they fork at once, as it is; what says it is closed, to what closes it, once the
/// forks forking it are done; and when it was last forked. Kept in the daemon's [`table`], at the
/// key of the store it is, of which no connection has anything open: what holds the store's path
/// to work on it, but to fork it, closes the kept store first ([`hold`], [`hold_to_write`]).
pub(super) struct Kept {
    store: Arc<HeldCell>,
    closed: futures::channel::oneshot::Receiver<std::convert::Infallible>,
    forked: std::time::Instant,
}

/// cladefs: the store kept open at `key`, if one is, forked now.
fn kept(key: &Path) -> Option<Arc<HeldCell>> {
    table().kept(key, |kept| {
        kept.forked = std::time::Instant::now();
        kept.store.clone()
    })
}

/// cladefs: when `open`, kept open at `key`, was last forked, if it is kept still.
fn forked(key: &Path, open: &Weak<HeldCell>) -> Option<std::time::Instant> {
    let is_open = |kept: &mut Kept| std::ptr::eq(Arc::as_ptr(&kept.store), open.as_ptr());
    table().kept(key, |kept| is_open(kept).then_some(kept.forked)).flatten()
}

/// cladefs: keeps `store`, opened to fork at the key `hold` holds, open for the forks after it
/// (TASK-92.5), until none forks it for [`LINGER`] ([`let_go_when_idle`]), a fork of it fails
/// ([`let_go_failed`]), or the daemon holds its path to work on it otherwise, or a connection to
/// write it. Its caller found nothing open there: were something, the table would refuse it, and
/// the kept store, not kept, would close once its caller's fork is done.
fn keep(hold: &Hold, store: OpenCell) -> Arc<HeldCell> {
    let (closing, closed) = futures::channel::oneshot::channel();
    // What says it is closed goes after the store.
    let store = Arc::new(HeldCell {
        store: Arc::new(store),
        _then: Some(Then::SayClosed { _closing: closing }),
    });
    let kept = Kept { store: store.clone(), closed, forked: std::time::Instant::now() };
    match hold.keep(kept) {
        Ok(()) => {
            let idle = let_go_when_idle(hold.key().to_owned(), Arc::downgrade(&store));
            // On the runtime that lives as long as the daemon, whatever its caller's.
            drop(super::server::IO.spawn(idle));
        }
        Err(_) => log::error!(
            "cladefsd: {} has something open already: the kept opened isn't kept",
            hold.key().display()
        ),
    }
    store
}

/// cladefs: closes `kept`, if given, a store kept open its holder took, and returns once it is
/// closed: once the forks forking it are done. It blocks.
fn close_kept_blocking(kept: Option<Kept>) {
    let Some(Kept { store, closed, .. }) = kept else { return };
    drop(release_held_behind(store));
    local_cell::blocks!("a kept store's close, waiting for the forks forking it");
    // cladefs: outside the simulation: the FUSE mount's hold, and its blocking thread.
    let _ = futures::executor::block_on(closed);
}

/// cladefs: closes `kept`, as [`close_kept_blocking`] does, holding no thread while it waits.
async fn close_kept(kept: Option<Kept>) {
    let Some(Kept { store, closed, .. }) = kept else { return };
    drop(release_held_behind(store));
    let _ = closed.await;
}

/// cladefs: closes every store kept open for forks, as the daemon drains (TASK-141.4), each under the
/// daemon's hold of its path, once the forks forking it are done, as [`let_go_when_idle`] closes
/// one: those it takes no hold of, logged, are closed past the drain's bound, if at all.
pub(super) async fn close_all_kept() {
    for key in table().kept_keys() {
        if let Err(errno) = hold(&key).await {
            log::warn!("cladefsd: closing the kept at {}: {errno}", key.display());
        }
    }
}

/// cladefs: closes `kept`, kept open at `key`, once none has forked it for [`LINGER`], under the
/// daemon's hold of its path, so that no fork opens it anew as it closes; unless something closed
/// it before, as a connection's opening it does, which this then leaves to it.
async fn let_go_when_idle(key: PathBuf, kept: Weak<HeldCell>) {
    loop {
        let Some(last) = forked(&key, &kept) else { return };
        match LINGER.checked_sub(last.elapsed()).filter(|left| !left.is_zero()) {
            Some(left) => tokio::time::sleep(left).await,
            None => {
                let Ok(hold) = hold_to_fork(&key).await else { return };
                if forked(&key, &kept).is_some_and(|last| last.elapsed() >= LINGER) {
                    return close_kept(hold.take_kept()).await;
                }
            }
        }
    }
}

/// cladefs: closes `kept`, kept open at `key`, if it is kept still, one of its forks having failed,
/// as all do once another placement takes its cell: the next fork opens it anew, as each did
/// before kept stores were kept. Under the daemon's hold of its path, as [`let_go_when_idle`] closes
/// one; its caller holds the kept store no more.
async fn let_go_failed(key: &Path, kept: Weak<HeldCell>) {
    if forked(key, &kept).is_none() {
        return;
    }
    let Ok(hold) = hold_to_fork(key).await else { return };
    if forked(key, &kept).is_some() {
        close_kept(hold.take_kept()).await;
    }
}

/// A store open for connections, as a request works on it, which it leaves open.
pub(super) struct Live(pub(super) Arc<OpenCell>);

impl Close for Live {
    /// Lets the store go, which its connection, gone meanwhile, may have left it last to close.
    async fn close(self) {
        release(self.0).await;
    }
}

impl Capacity for Live {
    fn capacity(&self) -> Result<(u64, u64), Errno> {
        capacity(&self.0)
    }

    async fn set_capacity(&self, capacity: u64) -> Result<(), Errno> {
        set_capacity(&self.0, capacity).await
    }
}

/// `store`'s capacity, and the bytes it holds.
fn capacity(store: &OpenCell) -> Result<(u64, u64), Errno> {
    let info = store.info()?;
    Ok((info.capacity, info.capacity - info.available))
}

/// Sets `store`'s capacity to `capacity` bytes.
async fn set_capacity<S>(store: &Arc<S>, capacity: u64) -> Result<(), Errno>
where
    S: Borrow<OpenCell> + Send + Sync + 'static,
{
    (**store).borrow().set_capacity(capacity).await
}

/// The store a fork forks ([`Here`]), and whether connections have it open: their writer's,
/// live, or their followers' (backlog doc-8), rather than opened here.
struct Forked {
    store: Here,
    live: bool,
}

impl Close for Forked {
    async fn close(self) {
        self.store.close().await;
    }
}

/// Returns once every commit of `store` ordered so far is uploaded: its flush, then the wait for
/// its uploads, holding no thread.
pub(super) async fn uploaded<S>(store: &Arc<S>) -> Result<(), Errno>
where
    S: Borrow<OpenCell> + Send + Sync + 'static,
{
    let epoch = (**store).borrow().make_local().await?;
    (**store).borrow().uploaded(epoch).await
}

/// The store a connection has open at `path`, if one has, whose VM process isn't gone: one going
/// is let go once its last commits are durable, which [`hold`] waits for. cladefs: or, with no
/// writer's connection, its followers' (backlog doc-8).
fn open_by_connection(path: &Path) -> Option<Arc<OpenCell>> {
    let key = store_key(path);
    match table().writer(&key) {
        Some(writer) if writer.going() => None,
        Some(writer) => writer.store().cloned(),
        None => table().instance(&key).map(|(store, _)| store),
    }
}

/// A store a VM process's opening makes, if there is none ([`open_store`]).
#[derive(Clone, Copy)]
pub(super) struct New {
    /// Its bytes.
    pub capacity: u64,
    /// What it holds.
    pub kind: Kind,
    /// The tenant whose new cell it is the one cell of (TASK-364): the connection's.
    pub tenant: u64,
}

/// The store at `path`, made as `new` says if given and it doesn't exist, keeping its cell in
/// the daemon's bucket, and its own extents' pages in `arena`, which it holds, or in the default
/// arena, without one ([`OpenCell::open_in`]).
pub(super) async fn open_store(
    path: &Path,
    new: Option<New>,
    arena: Option<&Arc<Arena>>,
) -> Result<OpenCell, Errno> {
    let arena = arena.cloned();
    let own = arena.clone();
    let store = attached(path, async |keeping| {
        let opening = Opening { arena: own, profiled: true, keeping };
        match new {
            Some(New { capacity, kind, tenant }) if !host_fs::exists(&path) => {
                let of = CellOf::New(tenant);
                OpenCell::create_kind_with(path, capacity, (of, kind), opening).await
            }
            _ => OpenCell::open_with(path, opening).await,
        }
    })
    .await?;
    // cladefs: a fork attached to it opens when first read through to, awaited (doc-24, step 5).
    store.open_bases(Arc::new(move |base| {
        let (base, arena) = (base.to_owned(), arena.clone());
        Box::pin(async move { self::base(&base, arena.as_ref()).await })
    }))?;
    Ok(store)
}

/// cladefs: the fork at `path`, attached to stores (backlog doc-10), for a store whose pages are of
/// `arena` to read through to: the one instance open of it, for all that attach it, in the default
/// arena, if public, or `arena`: `EBUSY` in another's, whose pages the connection's VM process
/// doesn't hold.
async fn base(path: &Path, arena: Option<&Arc<Arena>>) -> Result<Arc<OpenCell>, Errno> {
    let open = |found: Option<(Arc<OpenCell>, bool)>| match found {
        Some((_, true)) => Err(Errno::EBUSY),
        Some((store, false)) if store.is_public() => Ok(Some(store)),
        Some((store, false)) => {
            let own = arena.map_or(crate::pages::ArenaId::DEFAULT, |arena| arena.id());
            if store.arena() == own { Ok(Some(store)) } else { Err(Errno::EBUSY) }
        }
        None => Ok(None),
    };
    let key = store_key(path);
    if let Some(store) = open(table().instance(&key))? {
        return Ok(store);
    }
    let hold = hold(&key).await?;
    if let Some(store) = open(hold.instance())? {
        return Ok(store);
    }
    if !host_fs::exists(&path) {
        return Err(Errno::ENOENT);
    }
    let opened = attached(path, async |keeping| {
        OpenCell::open_base_with(path, arena.cloned(), keeping).await
    });
    let store = Arc::new(opened.await?);
    hold.open(&store)?;
    Ok(store)
}

/// cladefs: attaches the fork at `base` to the store at `path` (`AttachBase`), a VM's or stopped:
/// `EACCES` unless `allowed` says its tenant may read a store that isn't public.
pub(super) async fn attach(
    path: PathBuf,
    base: PathBuf,
    allowed: impl FnOnce(&Path, &Path) -> bool + Send + 'static,
) -> Result<(), Errno> {
    let base = host_fs::canonicalize(&base).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => Errno::ENOENT,
        _ => Errno::EIO,
    })?;
    let (public, ancestry) = match table().instance(&store_key(&base)) {
        Some((store, _)) => (store.is_public(), store.ancestry()),
        None => {
            let here = Here::open(&base, hold(&base).await?).await?;
            let what = (here.store().store.is_public(), here.store().store.ancestry());
            here.close().await;
            what
        }
    };
    if !public && !allowed(&store_key(&path), &base) {
        return Err(Errno::EACCES);
    }
    if let Some(store) = open_by_connection(&path) {
        return blocking(move || store.attach_base(&base, &ancestry)).await;
    }
    let held = hold(&path).await?;
    blocking(move || {
        let _held = held;
        if !host_fs::exists(&path) {
            return Err(Errno::ENOENT);
        }
        crate::cell_dir::attach(&path, &base, &ancestry)
    })
    .await
}

/// The store at `path`, as `open` opens it with the daemon's uploader's options ([`keeping`]),
/// keeping its cell in the bucket its description names: the daemon's, for the stores it made,
/// or another's, as an image's file bucket, for one it serves (TASK-165.6). Its opening takes the
/// cell's head (TASK-165.9).
async fn attached<T>(
    path: &Path,
    open: impl AsyncFnOnce(local_cell::upload::Options) -> Result<T, crate::Error>,
) -> Result<T, Errno> {
    // The daemon's own first, which the stores naming it share (`host_cell::bucket`).
    daemon_bucket()?;
    let keeping = keeping()?;
    open(keeping).await.map_err(|err| errno_of(format_args!("opening {}", path.display()), err))
}

/// How the daemon's stores' uploaders keep their cells in their buckets: with this host's
/// placement's token, for a kept store never placed; the host's cache of the bucket's objects; and the
/// upload interval ([`upload_interval`]). cladefs: and what holds the parts their segments pack as
/// they fill, which the daemon's memory governor lets go of first (`memory::held_parts`,
/// TASK-400).
fn keeping() -> Result<local_cell::upload::Options, Errno> {
    let token = crate::bucket::host_token().map_err(|err| {
        log::error!("cladefsd: {err}");
        Errno::EINVAL
    })?;
    let (cache, interval) = (part_cache()?, upload_interval());
    let held = Some(super::memory::held_parts().clone());
    // cladefs: and the hosts its replicated cells' configurations are made of (doc-21).
    let members = crate::bucket::members().map(local_cell::upload::MembersOf);
    let replace_after = replace_after().map_err(|err| {
        log::error!("cladefsd: {err}");
        Errno::EINVAL
    })?;
    Ok(local_cell::upload::Options {
        cache,
        held,
        token,
        interval,
        members,
        replace_after,
        ..Default::default()
    })
}

/// cladefs: how long a member of a replicated cell's configuration is down before its writer
/// replaces it, and the cell degraded before it drops to f = 0 (doc-21, TASK-537.3.5): the
/// seconds `CLADEFSD_REPLACE_AFTER` says, a positive number, or 30. Read at the daemon's start, to
/// refuse what it can't use, and as each store opens.
pub fn replace_after() -> Result<std::time::Duration, String> {
    let Some(set) = node_local::env::var_os("CLADEFSD_REPLACE_AFTER") else {
        return Ok(local_cell::upload::Options::default().replace_after);
    };
    match set.to_str().and_then(|seconds| seconds.parse::<u64>().ok()) {
        Some(0) | None => {
            Err(format!("CLADEFSD_REPLACE_AFTER must be a positive number of seconds: {set:?}"))
        }
        Some(seconds) => Ok(std::time::Duration::from_secs(seconds)),
    }
}

/// How long the stores' durable commits wait at most before they upload: the uploader's default;
/// in a test that asked for it, until something waits for an upload ([`uploads_when_asked`]).
fn upload_interval() -> std::time::Duration {
    #[cfg(test)]
    if UPLOADS_WHEN_ASKED.load(std::sync::atomic::Ordering::Relaxed) {
        return std::time::Duration::MAX;
    }
    local_cell::upload::Options::default().interval
}

/// The store at `path` as the daemon opens it to work on it, not a VM: its pages of the default
/// arena, which reads no boot profile into the cache ([`OpenCell::open_without_profile`]).
async fn open_here(path: &Path) -> Result<OpenCell, Errno> {
    attached(path, async |keeping| {
        OpenCell::open_with(path, Opening { arena: None, profiled: false, keeping }).await
    })
    .await
}

/// The errno of `err`, which doing `what` failed with: its own; `EBUSY` if it gave up waiting for
/// another process's lock of the store; or `EIO`, logged.
pub(super) fn errno_of(what: std::fmt::Arguments<'_>, err: crate::Error) -> Errno {
    // Another process's lock of the store, waited for: `EBUSY`, as for a connection's.
    if err.is_busy() {
        log::debug!("cladefsd: {what}: {err:#}");
        return Errno::EBUSY;
    }
    err.errno().unwrap_or_else(|| {
        log::warn!("cladefsd: {what}: {err:#}");
        Errno::EIO
    })
}

/// Makes an empty store at `path`, which must not exist (`EEXIST`), of `capacity` bytes, and
/// returns its cell: a new one, or `cell`, if given, whose store kept in the daemon's bucket
/// the daemon takes once it opens it (`cladefsctl take`); placed at `token` if given ([`place`]).
/// cladefs: made as [`OpenCell::create_placed`] makes it, its head taken with `token`, not the
/// daemon's, so that a kept store placed later than the one on another host that took the cell last
/// takes it over (TASK-266: `ESTALE`, its head taken with the daemon's token before it was
/// placed).
pub(super) async fn create(
    path: PathBuf,
    capacity: u64,
    cell: CellId,
    (token, kind, replication, force): (Option<u64>, Kind, Option<bucket_cell::layout::Wish>, bool),
) -> Result<CellId, Errno> {
    // Let go of once the work is done.
    let _held = hold(&path).await?;
    // cladefs: what a making its host died in left, unmade, is no store: made anew (TASK-196).
    let checked = path.clone();
    blocking(move || {
        removed_if_unmade(&checked)?;
        if host_fs::exists(&checked) {
            return Err(Errno::EEXIST);
        }
        Ok(())
    })
    .await?;
    // cladefs: a cell of 0 is a new one, its ID, its cell's too, the bucket's to give: no
    // caller names a new cell; a cell named, one the bucket has, of a cell named (TASK-364).
    let of = match (cell.id, cell.id) {
        (0, 0) => CellOf::New(cell.tenant),
        (_, 0) | (0, _) => return Err(Errno::EINVAL),
        (_, _) => CellOf::Taken(cell),
    };
    // cladefs: the replication its call asked for, which its taking gives the cell's head
    // (doc-21, TASK-537.3.1); and its taking forced, if asked (TASK-681).
    attached(&path, async |keeping| {
        let keeping = local_cell::upload::Options { replication, force, ..keeping };
        OpenCell::create_placed(&path, (capacity, of, token), kind, keeping).await
    })
    .await
}

/// Makes a store at `path`, which must not exist (`EEXIST`), of `capacity` bytes, holding a
/// filesystem of the container image at `image` (`cladefs::import::store`), the one cell of a new
/// cell of `tenant`'s (TASK-364), and returns its cell.
pub(super) async fn import(
    path: PathBuf,
    capacity: u64,
    image: PathBuf,
    reference: Option<String>,
    tenant: u64,
) -> Result<CellId, Errno> {
    let _held = hold(&path).await?;
    // cladefs: what an import its host died in left, unmade, is no store: made anew
    // (TASK-196).
    let checked = path.clone();
    blocking(move || {
        removed_if_unmade(&checked)?;
        if host_fs::exists(&checked) {
            return Err(Errno::EEXIST);
        }
        Ok(())
    })
    .await?;
    // As the daemon's stores keep their cells: with the host's cache, which what the bucket
    // acknowledged of the image goes into and the import's check reads (TASK-237).
    // cladefs: on a blocking thread of its own, as before: its walk of a layer's tar entries, which
    // are not `Send`, spans its awaits (doc-24: image import stays outside the simulation).
    let (at, keeping) = (path.clone(), keeping()?);
    blocking(move || {
        let imported = (image.as_path(), reference.as_deref());
        let made = crate::import::store_keeping(&at, (capacity, tenant), imported, keeping);
        outside_the_simulation(made)
            .map_err(|err| errno_of(format_args!("importing {}", image.display()), err))
    })
    .await?;
    let store = open_here(&path).await?;
    let cell = store.cell();
    store.close().await;
    Ok(cell)
}

/// cladefs: awaits `future`, blocking the calling thread, a blocking thread of the runtime's: what an
/// image's import does, whose walk of a layer's tar entries, which are not `Send`, spans its awaits,
/// and which stays outside the simulation (doc-24). Under madsim, which runs on one thread, it fails
/// at once.
fn outside_the_simulation<F: std::future::Future>(future: F) -> F::Output {
    local_cell::blocks!(
        "an image's import, outside the simulation (cells::outside_the_simulation)"
    );
    futures::executor::block_on(future)
}

/// The store at `path`, or, if it doesn't exist, one of `cell`, of `capacity` bytes, taken from
/// the daemon's bucket: what the cell holds there.
/// cladefs: a store of `cell` there whose write-ahead log another writer's commits moved its cell
/// past is opened as the cell is, the log dropped (`OpenCell::take_with`, TASK-165.9).
async fn take_store(path: &Path, cell: Option<CellId>, capacity: u64) -> Result<OpenCell, Errno> {
    let cell = match cell {
        Some(cell) => cell,
        None if !host_fs::exists(&path) => return Err(Errno::ENOENT),
        None => local_cell::host_cell(path).map_err(|_| Errno::ENOENT)?,
    };
    attached(path, async |keeping| {
        let opening = Opening { arena: None, profiled: false, keeping };
        OpenCell::take_with(path, (capacity, cell, None), opening).await
    })
    .await
}

/// Removes what a store's making, or a fork's, left at `path`, unmade, its host crashing
/// (`host_cell::unmade`): no store, which a fork or a restore to `path` makes anew. Its holder
/// holds `path`.
pub(super) fn removed_if_unmade(path: &Path) -> Result<(), Errno> {
    if !crate::host_cell::unmade(path) {
        return Ok(());
    }
    local_cell::remove_store_dir(path).map_err(|err| {
        log::warn!("cladefsd: removing {}, a store unmade: {err:#}", path.display());
        Errno::EIO
    })
}

/// cladefs: where the daemon keeps the version `version` of `cell`'s cell open for the forks of it
/// made here ([`host_cell::version_dir`]), made from the bucket the first time the version is
/// forked here ([`opened_version`]).
fn version_dir(cell: CellId, version: u64) -> PathBuf {
    crate::host_cell::version_dir(cell, version)
}

/// cladefs: the directory the version `version` of `cell` is kept open in for forks here
/// ([`version_dir`]), made if it isn't, of `capacity` bytes, a block at least, as a restore asking
/// none for the version's own gives: from the bucket, once the cell's commits here, if its writer
/// is here, are uploaded, the version's among them, as a version is local as it is made and
/// uploaded behind. `ENOENT` if the bucket has no such cell.
async fn opened_version(cell: CellId, version: u64, capacity: u64) -> Result<PathBuf, Errno> {
    let capacity = capacity.max(OpenCell::BLOCK_SIZE as u64);
    let dir = version_dir(cell, version);
    let held = hold_within(&dir, VERSION_WAIT).await?;
    if host_fs::exists(&dir) && !crate::host_cell::unmade(&dir) {
        return Ok(dir);
    }
    // Their paths alone: a reference to a store kept across its sync would keep it from closing
    // (TASK-648).
    let writers: Vec<PathBuf> = (table().instances().into_iter())
        .filter(|(_, store)| store.cell() == cell)
        .map(|(path, _)| path)
        .collect();
    for path in writers {
        sync(path).await?;
    }
    // A store its connection let go of is none of those, while it closes on a blocking thread,
    // uploading its last commits, the version's among them: its close waited for, if the version
    // was made here, before the bucket is read for it (TASK-644).
    if let Some(of) = crate::host_cell::version_made_of(cell, version) {
        local_cell::closed(&of).await.map_err(|err| {
            log::warn!("cladefsd: waiting for {} to close: {err}", of.display());
            Errno::EBUSY
        })?;
    }
    let bucket = daemon_bucket()?;
    let found = bucket_cell::head::get(&*bucket, cell).await;
    let object = found.map_err(|_| Errno::EIO)?.ok_or(Errno::ENOENT)?;
    let kind = object.kind.ok_or(Errno::ENOENT)?;
    let made = dir.clone();
    blocking(move || {
        let _held = held;
        removed_if_unmade(&made)?;
        if let Some(root) = made.parent() {
            host_fs::create_dir_all(root).map_err(|_| Errno::EIO)?;
        }
        let url = crate::host_cell::bucket_for(&made).map_err(|_| Errno::EIO)?;
        let description =
            local_cell::Description { block_size: OpenCell::BLOCK_SIZE, capacity, cell };
        OpenCell::create_at_version(&made, (&url, kind), description, version).map_err(|err| {
            errno_of(format_args!("opening version {version} of {cell} here"), err)
        })?;
        // What is attached to the store the version was made of, if made here, is attached to
        // the version's forks, a restored VM's too.
        match crate::host_cell::version_made_of(cell, version) {
            Some(of) => crate::cell_dir::inherit_attached(&of, &made).map_err(|err| {
                errno_of(
                    format_args!("inheriting what is attached to {}", of.display()),
                    err.into(),
                )
            }),
            None => Ok(()),
        }
    })
    .await?;
    Ok(dir)
}

/// Forks the version `version` of `cell`, a VM snapshot's, to a new cell at `path`, which the VM
/// restored from the snapshot opens (TASK-39.1). Restores of one snapshot, a zygote's, fork the
/// version at once, kept open for them here ([`opened_version`], TASK-92.5). Its waits hold no
/// thread; its fork blocks. A version kept open whose fork fails, as once another host took its
/// cell, is let go of and opened anew for one more fork. The fork is a new cell of `tenant`'s, if
/// given, the granted tenant's whose VM process restores, or else of `cell`'s tenant's (TASK-364):
/// one of another tenant's than `cell`'s only if the version is shared (`EACCES` otherwise).
pub(super) async fn fork_version(
    (cell, version): (CellId, u64),
    capacity: u64,
    (path, tenant): (&Path, Option<u64>),
) -> Result<(), Errno> {
    let from = opened_version(cell, version, capacity).await?;
    match fork_kept(&from, capacity, (path, tenant)).await {
        (Err(errno), Opened::Before) if store_at_fault(errno) => {
            // What a crash left at `path` goes first, as before any restore's.
            removed_if_unmade(path)?;
            fork_kept(&from, capacity, (path, tenant)).await.0
        }
        (forked, _) => forked,
    }
}

/// cladefs: forks the version `version` of `cell` to a new store at `to`, a new cell of `tenant`'s
/// if given, else of `cell`'s tenant's, as [`fork_version`] does, the store at `to` held throughout
/// (`ForkCell`'s `version`): returns its cell.
pub(super) async fn fork_of_version(
    cell: CellId,
    version: u64,
    to: PathBuf,
    tenant: Option<u64>,
) -> Result<CellId, Errno> {
    let _child = hold_to_fork(&to).await?;
    fork_version((cell, version), OpenCell::BLOCK_SIZE as u64, (&to, tenant)).await?;
    local_cell::host_cell(&to).map_err(|_| Errno::EIO)
}

/// cladefs: forks the version `version` of `cell` to `to`, an ephemeral cell kept on this host alone
/// (doc-25, TASK-698.3; `ForkCell`'s `ephemeral`), a new cell of `tenant`'s if given, else of
/// `cell`'s tenant's, from the store kept open of the version, as [`fork_of_version`] forks it, the
/// store at `to` held throughout: returns its cell.
pub(super) async fn fork_ephemeral_of_version(
    cell: CellId,
    version: u64,
    to: PathBuf,
    tenant: Option<u64>,
) -> Result<CellId, Errno> {
    let _child = hold_to_fork(&to).await?;
    let capacity = OpenCell::BLOCK_SIZE as u64;
    let from = opened_version(cell, version, capacity).await.inspect_err(|errno| {
        log::warn!("cladefsd: version {version:x} of {cell} for an ephemeral fork: {errno:?}")
    })?;
    let (kept, _) = kept_or_open(&from, capacity).await.inspect_err(|errno| {
        log::warn!("cladefsd: opening {} for an ephemeral fork: {errno:?}", from.display())
    })?;
    let tenant = tenant.unwrap_or(kept.store.cell().tenant);
    let forked = kept.store.fork_ephemeral(&to, tenant).await.map_err(|err| {
        errno_of(format_args!("forking {} ephemerally to {}", from.display(), to.display()), err)
    });
    release_held(kept).await;
    forked
}

/// cladefs: `Ok` if `store`, at a restore's `path` already, holds what the version `version` of
/// `cell` does ([`OpenCell::holds_what`]): the store the snapshot was taken of, written nothing
/// since, or a fork of the version an earlier restore made, its VM having written nothing
/// (TASK-194). `EEXIST` otherwise, logged: its guest, of the snapshot's memory, would read extents
/// freed since and commit over what was written since. Nothing of the store is changed.
pub(super) async fn at_version<S>(
    (cell, version): (CellId, u64),
    capacity: u64,
    path: &Path,
    store: &Arc<S>,
) -> Result<(), Errno>
where
    S: Borrow<OpenCell> + Send + Sync + 'static,
{
    // The snapshotted store itself: its commits uploaded first, the version's among them, as a
    // version is local as it is made and uploaded behind.
    if (**store).borrow().cell() == cell {
        uploaded(store).await?;
    }
    let from = opened_version(cell, version, capacity).await?;
    let (kept, _) = kept_or_open(&from, capacity).await?;
    let holds = (**store).borrow().holds_what(&kept.store);
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

/// cladefs: forks the kept store at `from` to `path`, as [`fork_version`] does, once: and says when the
/// kept store was opened. A kept store whose fork fails is let go of ([`let_go_failed`]).
async fn fork_kept(
    from: &Path,
    capacity: u64,
    (path, tenant): (&Path, Option<u64>),
) -> (Result<(), Errno>, Opened) {
    let (kept, opened) = match kept_or_open(from, capacity).await {
        Ok(kept) => kept,
        Err(errno) => return (Err(errno), Opened::Now),
    };
    // A new cell of `tenant`'s, or of the kept store's tenant's (TASK-364).
    let tenant = tenant.unwrap_or(kept.store.cell().tenant);
    let failed = Arc::downgrade(&kept);
    // cladefs: awaited, its disk work off the runtime's threads (TASK-685.3).
    let forked = kept.store.fork(path, tenant).await.map(drop).map_err(|err| {
        errno_of(format_args!("forking {} to {}", from.display(), path.display()), err)
    });
    release_held(kept).await;
    if forked.is_err_and(store_at_fault) {
        let_go_failed(&store_key(from), failed).await;
    }
    (forked, opened)
}

/// cladefs: whether a fork that failed with `errno` may have failed by the store it forks, as a
/// store kept open whose cell another host took, which is let go of, and opened anew for a fork
/// once more: not by its child's path being taken (`EEXIST`), nor by the fork being refused, as of
/// another tenant's kept store not shared (`EACCES`, TASK-395.7), which no store opened anew changes.
fn store_at_fault(errno: Errno) -> bool {
    !matches!(errno, Errno::EEXIST | Errno::EACCES)
}

/// cladefs: when a restore's kept store was opened ([`kept_or_open`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Opened {
    /// For it, or for the restores beside it.
    Now,
    /// For a restore before it, kept open since ([`keep`]).
    Before,
}

/// cladefs: the kept store at `from` for a restore to fork, of `capacity` bytes if the daemon takes its
/// cell from its bucket into the store there ([`take_store`]), which forks fork at once
/// (TASK-92.5): the store kept open, if one is ([`kept`]); or, under the daemon's hold of its path,
/// which no connection opens it meanwhile, the store followers have open, forked as it is, held
/// until the fork is done; or the kept store opened, kept open from then on ([`keep`]). And when it was
/// opened. A restore waiting for another's opening of it holds no thread.
async fn kept_or_open(from: &Path, capacity: u64) -> Result<(Arc<HeldCell>, Opened), Errno> {
    if let Some(kept) = kept(&store_key(from)) {
        return Ok((kept, Opened::Before));
    }
    // TASK-443.6: however long a restore beside it takes to open it ([`VERSION_WAIT`]).
    let hold = table().hold_async(&store_key(from), VERSION_WAIT).await?;
    // Opened by a restore that held it before, beside this one.
    if let Some(kept) = kept(hold.key()) {
        return Ok((kept, Opened::Now));
    }
    if let Some((store, _)) = hold.instance() {
        return Ok((Arc::new(HeldCell::new(store, Some(hold))), Opened::Now));
    }
    // cladefs: awaited, holding no worker of the control runtime's (TASK-688).
    Ok((keep(&hold, take_store(from, None, capacity).await?), Opened::Now))
}

/// Forks the store at `from` to a new cell at `to`, and returns its cell: live, if a connection
/// has it open, once nothing is open (`EBUSY` otherwise); or its followers, with no writer; or the
/// store kept open of it, as restores fork it (TASK-92.5), opened here if none is, and kept open for
/// the forks after it. Its commits' uploads, which the fork follows, are waited for first, holding
/// no thread; the fork, as of then, is made on a blocking thread. The store at `to` is held
/// throughout, so that a connection opens the child only once the fork is made, in the bucket too
/// (TASK-79.95). A store kept open whose fork fails, as once another host took its cell, is opened
/// anew for one more fork ([`fork_version`], TASK-79.103). The child is a new cell of `tenant`'s, if
/// given: the granted tenant's whose directory `to` is in; or else of `from`'s tenant's (TASK-364).
/// A version not shared forks to no cell of another tenant's (`EACCES`, TASK-395.7); nor does a
/// store with a writer, unless it is the default tenant's, as the daemon's own stores and public
/// bases are (`EACCES`, TASK-395.9), refused before anything is made.
pub(super) async fn fork(from: PathBuf, to: PathBuf, tenant: Option<u64>) -> Result<CellId, Errno> {
    fork_from_store(&from, &to, tenant).await
}

/// cladefs: forks the store at `from` to `to` as [`fork`] does.
async fn fork_from_store(from: &Path, to: &Path, tenant: Option<u64>) -> Result<CellId, Errno> {
    let _child = hold_to_fork(to).await?;
    let key = store_key(from);
    // A new cell of `tenant`'s, or of the store's tenant's (TASK-364).
    let from_cell = local_cell::host_cell(from).map_err(|_| Errno::ENOENT)?;
    let tenant = tenant.unwrap_or(from_cell.tenant);
    let was_kept = kept(&key).is_some();
    match fork_held(from, to, tenant, &key).await {
        Err(errno) if was_kept && store_at_fault(errno) => fork_held(from, to, tenant, &key).await,
        forked => forked,
    }
}

/// cladefs: forks the store at `from`, whose key is `key`, to `to`, which the caller holds, as
/// [`fork`] does, once, to a new cell of `tenant`'s, and returns its cell. A kept store whose fork fails is let go of ([`let_go_failed`]).
async fn fork_held(from: &Path, to: &Path, tenant: u64, key: &Path) -> Result<CellId, Errno> {
    let live = |connection: &Connection| {
        Some(Forked { store: Here::of(connection.store()?.clone(), None), live: true })
    };
    let open = |hold: Option<Hold>| async {
        if let Some((store, _)) = table().instance(key) {
            return Ok(Forked { store: Here::of(store, hold), live: true });
        }
        // Opened by a restore that held it before.
        if let Some(kept) = kept(key) {
            return Ok(Forked { store: Here::kept(kept), live: false });
        }
        let store = open_here(from).await?;
        let store = match hold {
            // Let go of once kept.
            Some(hold) => Here::kept(keep(&hold, store)),
            None => Here::of(Arc::new(store), None),
        };
        Ok(Forked { store, live: false })
    };
    let forking = match kept(key) {
        Some(kept) => Forked { store: Here::kept(kept), live: false },
        None => cell_requests::forking(table(), key, live, CLOSING_WAIT, open).await?,
    };
    let (store, live) = (forking.store.store().clone(), forking.live);
    let held = Arc::downgrade(&store);
    let to = to.to_owned();
    let forked = async {
        uploaded(&store).await?;
        fail::fail_point!("cladefsd::fork_uploaded", |_| Err(Errno::EIO));
        let forked = match live {
            true => store.store.fork_quiescent(&to, tenant).await,
            false => store.store.fork(&to, tenant).await,
        };
        forked.map_err(|err| errno_of(format_args!("forking to {}", to.display()), err))
    }
    .await;
    release_held(store).await;
    forking.close().await;
    if forked.as_ref().is_err_and(|errno| store_at_fault(*errno)) {
        let_go_failed(key, held).await;
    }
    forked
}

/// cladefs: awaits `work` with the store at `path`: the one a connection has open, if one has; or
/// opened here, which no connection opens meanwhile, and closed after (TASK-685.3).
async fn with_store<T>(
    path: &Path,
    work: impl AsyncFnOnce(&OpenCell) -> Result<T, Errno>,
) -> Result<T, Errno> {
    if let Some(store) = open_by_connection(path) {
        let done = work(&store).await;
        release(store).await;
        return done;
    }
    let here = Here::open(path, hold(path).await?).await?;
    let done = work(&here.store().store).await;
    here.close().await;
    done
}

/// cladefs: makes a version of the cell of the store at `path` ([`OpenCell::make_version`]),
/// keeping its newest `keep_last` if given; returns its ID.
pub(super) async fn make_version(path: PathBuf, keep_last: Option<u32>) -> Result<u64, Errno> {
    with_store(&path, async |store| {
        let made = store.make_version(keep_last).await;
        made.map_err(|err| errno_of(format_args!("making a version of {}", path.display()), err))
    })
    .await
}

/// cladefs: the versions of the cell of the store at `path` ([`OpenCell::versions`]).
pub(super) async fn versions(path: PathBuf) -> Result<Vec<crate::VersionInfo>, Errno> {
    with_store(&path, async |store| Ok(store.versions().await)).await
}

/// cladefs: drops the versions `ids` of the cell of the store at `path`
/// ([`OpenCell::drop_versions`]).
pub(super) async fn drop_versions(path: PathBuf, ids: BTreeSet<u64>) -> Result<(), Errno> {
    with_store(&path, async |store| {
        let dropped = store.drop_versions(&ids).await;
        dropped
            .map_err(|err| errno_of(format_args!("dropping versions of {}", path.display()), err))
    })
    .await
}

/// cladefs: shares the version `id` of the cell of the store at `path`, or no longer
/// ([`OpenCell::share_version`]).
pub(super) async fn share_version(path: PathBuf, id: u64, shared: bool) -> Result<(), Errno> {
    with_store(&path, async |store| {
        let shared = store.share_version(id, shared).await;
        shared.map_err(|err| {
            errno_of(format_args!("sharing version {id} of {}", path.display()), err)
        })
    })
    .await
}

/// Waits until every commit of the store at `path` is in the daemon's bucket, holding no thread:
/// through the connection's store, if one has it open; or opened here, which uploads what it
/// holds, if none has.
pub(super) async fn sync(path: PathBuf) -> Result<(), Errno> {
    if let Some(store) = open_by_connection(&path) {
        return uploaded(&store).await;
    }
    let held = hold(&path).await?;
    // A store its connection let go of may live on a while, its last reference dropped on a
    // blocking thread: synced as it is, under the hold, never opened a second time beside it,
    // which this process's claim on its directory refuses (TASK-648).
    if let Some((store, _)) = held.instance() {
        return uploaded(&store).await;
    }
    let here = Here::open(&path, held).await?;
    let synced = uploaded(here.store()).await;
    here.close().await;
    synced
}

/// Deletes the store at `path`, or the fork whose handle it is, and its cell in its bucket
/// ([`OpenCell::delete`], [`bucket_of`]): `EBUSY` while a connection has it open.
pub(super) async fn delete(path: PathBuf) -> Result<(), Errno> {
    let bucket = bucket_of(&path)?;
    delete_alone(path, Some(bucket)).await
}

/// The bucket the store at `path`, or the fork whose handle it is, keeps its cell in, as its
/// description names it (TASK-165.6): or, if its description is not there, as for what a
/// deletion that stopped partway left, which deletes nothing in a bucket, the daemon's.
fn bucket_of(path: &Path) -> Result<Arc<bucket_cell::object_bucket::ObjectBucket>, Errno> {
    match crate::host_cell::bucket_at(path) {
        Ok(Some(bucket)) => Ok(bucket),
        Ok(None) => daemon_bucket(),
        Err(err) => {
            log::warn!("cladefsd: the bucket of {}: {err:#}", path.display());
            Err(Errno::EIO)
        }
    }
}

/// cladefs: deletes this host's kept store of the store at `path`, or of the fork whose handle it is
/// (TASK-154): as [`delete`] does, but for its cell in the daemon's bucket, which stays, for what
/// else restores it: another host's kept store, or this one's taken again. What its commits hold that
/// the bucket doesn't goes with it. A kept store the VM manager took for a tenant goes so: the tenant
/// restores it no more, and the snapshot lives on.
pub(super) async fn drop_here(path: PathBuf) -> Result<(), Errno> {
    delete_alone(path, None).await
}

/// Deletes the store at `path`, and its cell in `bucket`, if given: `EBUSY` while a connection
/// has it open.
async fn delete_alone(
    path: PathBuf,
    bucket: Option<Arc<bucket_cell::object_bucket::ObjectBucket>>,
) -> Result<(), Errno> {
    let _held = hold_alone(&path).await?;
    let deleted = OpenCell::delete(&path, bucket.as_deref()).await;
    deleted.map_err(|err| errno_of(format_args!("deleting {}", path.display()), err))
}

/// Sets the capacity of the store at `path` to `capacity` bytes: through the connection that has
/// it open, if one does, which grows it, or shrinks it if its guest agrees, holding no thread while
/// it waits for the guest's answer; or, if none does, here, growing it (`EBUSY` to shrink it, its
/// guest not here to agree), no connection opening it meanwhile. cladefs: as the store's access says
/// ([`crate::CellAccess::may_resize`]): a kept store's capacity, as all else of it, changes nowhere
/// (`EROFS`).
pub(super) async fn resize(path: PathBuf, capacity: u64) -> Result<(), Errno> {
    let key = store_key(&path);
    let open = |hold: Option<Hold>| {
        let key = key.clone();
        async move {
            // cladefs: open for followers, it grows as it is; the store kept open of it for forks,
            // it opens anew, closed first (TASK-92.5): attached, as any store the daemon opens,
            // which says whether it is a kept store (TASK-160.10).
            let store = match table().instance(&key) {
                Some((store, _)) => store,
                None => {
                    close_kept(hold.as_ref().and_then(Hold::take_kept)).await;
                    if !host_fs::exists(&key) {
                        return Err(Errno::ENOENT);
                    }
                    Arc::new(open_here(&key).await?)
                }
            };
            Ok(Here::of(store, hold))
        }
    };
    cell_requests::resize(table(), &key, capacity, open).await
}

/// Places the store at `path`, or the fork whose handle it is, at the placement whose fencing
/// token is `token` ([`OpenCell::place`]), its takings forced if `force`
/// ([`OpenCell::place_forced`]): `EBUSY` while a connection has it open, whose writer took its
/// cell with the token it had.
pub(super) async fn place(path: PathBuf, token: u64, force: bool) -> Result<(), Errno> {
    let held = hold_alone(&path).await?;
    blocking(move || {
        let _held = held;
        let placed = match force {
            true => OpenCell::place_forced(&path, token),
            false => OpenCell::place(&path, token),
        };
        placed.map_err(|err| errno_of(format_args!("placing {}", path.display()), err))
    })
    .await
}

/// Makes the store at `path` a public base, and the cells it inherited from
/// ([`OpenCell::make_public`]); or, if `attach_only`, its cell alone, to attach to stores, not
/// to fork ([`OpenCell::make_public_attach_only`]): `EBUSY` while a connection has it open,
/// whose guest could write it still.
pub(super) async fn make_public(path: PathBuf, attach_only: bool) -> Result<(), Errno> {
    let _held = hold_alone(&path).await?;
    let store = Arc::new(open_here(&path).await?);
    let making = store.clone();
    let made = blocking(move || match attach_only {
        true => making.make_public_attach_only(),
        false => making.make_public(),
    })
    .await;
    if let Some(store) = Arc::into_inner(store) {
        store.close().await;
    }
    made
}

/// What the store at `path` is, and holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CellState {
    pub cell: CellId,
    pub info: ExtentInfo,
    /// What its cell holds, bytes and extents, and whose (TASK-49, TASK-454).
    pub holding: crate::CellHolding,
    /// Whether it is a public base.
    pub public: bool,
    /// Whether a connection has it open.
    pub open: bool,
    /// The epoch of its latest commit.
    pub committed: u64,
    /// What its uploader has done and has left to do, if its commits upload now.
    pub upload: Option<local_cell::upload::UploadStatus>,
}

/// What `store` is, and holds; `open` if a connection has it open.
fn state_of(store: &OpenCell, open: bool) -> Result<CellState, Errno> {
    Ok(CellState {
        cell: store.cell(),
        info: store.info()?,
        holding: store.holding()?,
        public: store.is_public(),
        open,
        committed: store.committed()?,
        upload: store.upload_status()?,
    })
}

/// The stores connections have open, writers' and followers', by their paths, with what each is
/// and holds (TASK-50.5).
pub(super) fn list() -> Result<Vec<(PathBuf, CellState)>, Errno> {
    let stores = table().instances();
    stores.into_iter().map(|(path, store)| Ok((path, state_of(&store, true)?))).collect()
}

/// What the store at `path` is, and holds: as the connection that has it open has it, if one does.
pub(super) async fn stat(path: PathBuf) -> Result<CellState, Errno> {
    if let Some(store) = open_by_connection(&path) {
        return state_of(&store, true);
    }
    let _held = hold(&path).await?;
    if !host_fs::exists(&path) {
        return Err(Errno::ENOENT);
    }
    let store = open_here(&path).await?;
    let state = state_of(&store, false);
    store.close().await;
    state
}

node_local::node_local! {
    /// The bucket [`daemon_bucket`] made, once a store needs it.
    static BUCKET: OnceLock<Arc<bucket_cell::object_bucket::ObjectBucket>> = OnceLock::new();
}

/// What the daemon did with its bucket so far, if it made it (`metrics`).
pub(super) fn bucket_stats() -> Option<bucket_cell::object_bucket::Stats> {
    BUCKET.get().get().map(|bucket| bucket.stats())
}

/// cladefs: what the daemon asked of each bucket, its own and those of other buckets' cells
/// (TASK-633.4), by number (`metrics`).
pub(super) fn bucket_stats_by_bucket() -> Vec<(u16, bucket_cell::object_bucket::Stats)> {
    BUCKET.get().get().map(|bucket| bucket.stats_by_bucket()).unwrap_or_default()
}

/// cladefs: the stats file's line of what the daemon asked of other buckets than its own
/// (TASK-633.4): each one's GETs, downloaded and uploaded bytes, by number.
fn buckets_line(bucket: &bucket_cell::object_bucket::ObjectBucket) -> String {
    use bucket_cell::object_bucket::{Request, Upload};
    let others: Vec<String> = (bucket.stats_by_bucket().into_iter().skip(1))
        .map(|(number, stats)| {
            let gets = stats.requests(Request::Get) + stats.requests(Request::GetRange);
            let uploaded: u64 = Upload::ALL.iter().map(|upload| stats.uploaded(*upload)).sum();
            format!("{number} gets={gets} downloaded={} uploaded={uploaded}", stats.downloaded)
        })
        .collect();
    match others.is_empty() {
        true => "buckets: its own alone".to_owned(),
        false => format!("buckets: {}", others.join("; ")),
    }
}

/// How many stores connections have open, to write (`metrics`).
pub(super) fn open_count() -> usize {
    table().writers()
}

/// cladefs: the stores open for connections, writers' and followers'.
fn open_instances() -> Vec<Arc<OpenCell>> {
    table().instances().into_iter().map(|(_, store)| store).collect()
}

/// The cells of the stores open for connections whose commits upload now, and what their
/// uploaders have left to do (`metrics`, TASK-91.3).
pub(super) fn upload_statuses() -> Vec<(CellId, local_cell::upload::UploadStatus)> {
    (open_instances().iter())
        .filter_map(|store| Some((store.cell(), store.upload_status().ok()??)))
        .collect()
}

/// How many views followers have of the stores open for connections (`metrics`): counted of the
/// views alone, which hold none of the stores' state, their guests' (TASK-92.17).
pub(super) fn follower_views() -> usize {
    open_instances().iter().map(|store| store.view_stats().views).sum()
}

/// cladefs: the bucket `CLADEFSD_BUCKET` names, which the stores opened keep their cells in:
/// `EIO` if it names none, as the daemon's host must (`crate::bucket::Host`, TASK-45).
pub(crate) fn daemon_bucket() -> Result<Arc<bucket_cell::object_bucket::ObjectBucket>, Errno> {
    if let Some(bucket) = BUCKET.get().get() {
        return Ok(bucket.clone());
    }
    let host = crate::bucket::Host::from_env().map_err(|err| {
        log::error!("cladefsd: {err}");
        Errno::EIO
    })?;
    let bucket = crate::host_cell::bucket(&host.bucket).map_err(|err| {
        log::error!("cladefsd: CLADEFSD_BUCKET: {err:#}");
        Errno::EIO
    })?;
    Ok(BUCKET
        .get()
        .get_or_init(|| {
            watch_stats(Arc::downgrade(&bucket));
            bucket
        })
        .clone())
}

/// Whether the stores opened upload only when something waits for an upload ([`uploads_when_asked`]).
#[cfg(test)]
static UPLOADS_WHEN_ASKED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Has the stores opened from now on upload only when something waits for an upload (a fork, a
/// sync, a commit stalled at the lag's bound, the store closing), never as their interval runs
/// out, as `bucket::fork_faults`' stores do. For tests that set a failpoint to fail once for one
/// store's work: failpoints are the process's, and another store's uploader, woken by its
/// interval, would pass it first (TASK-157).
#[cfg(test)]
pub(super) fn uploads_when_asked() {
    UPLOADS_WHEN_ASKED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Makes `bucket` the daemon's, before any store opens: for tests that put themselves between the
/// daemon's stores and their objects. The stores naming `CLADEFSD_BUCKET`'s bucket open it
/// (`host_cell::use_bucket`).
#[cfg(any(test, madsim))]
pub(super) fn use_bucket(bucket: impl Into<Arc<bucket_cell::object_bucket::ObjectBucket>>) {
    let bucket = bucket.into();
    let host = crate::bucket::Host::from_env().expect("the daemon's host");
    crate::host_cell::use_bucket(&host.bucket, bucket.clone());
    assert!(BUCKET.get().set(bucket.clone()).is_ok(), "the daemon has its bucket already");
    watch_stats(Arc::downgrade(&bucket));
}

/// cladefs: how often the bucket's requests and bytes are looked at, and how often, at most, they
/// are logged while they change.
const STATS_POLL: std::time::Duration = std::time::Duration::from_millis(200);
const STATS_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
/// cladefs: how often the stats file is written again for its process's line alone, its CPU time
/// and memory, which change each poll: what its file says of the process is no older than this.
const PROCESS_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// cladefs: what the stores' followers are (backlog doc-8, TASK-42.7): their views now, and the
/// most views, commits behind, time behind and bytes of frees kept for them seen since the daemon
/// started, which the followers' measurements report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FollowerStats {
    views: usize,
    max_views: usize,
    max_behind: u64,
    max_stale_ms: u64,
    max_kept: u64,
}

impl FollowerStats {
    /// Takes the stores open for connections as they are now: those followers view alone, as
    /// those no follower views keep nothing for them, whose state their guests' requests hold is
    /// then taken for the bytes of frees views keep alone, which it counts (TASK-92.17).
    fn update(&mut self) {
        self.views = 0;
        for store in open_instances() {
            let views = store.view_stats();
            if views.views == 0 {
                continue;
            }
            self.views += views.views;
            self.max_behind = self.max_behind.max(views.behind);
            self.max_stale_ms = self.max_stale_ms.max(views.stale_for.as_millis() as u64);
            if let Ok(kept) = store.kept_for_views() {
                self.max_kept = self.max_kept.max(kept);
            }
        }
        self.max_views = self.max_views.max(self.views);
    }

    fn line(&self) -> String {
        format!(
            "followers: views={} max-views={} max-behind={} max-stale-ms={} max-kept={}",
            self.views, self.max_views, self.max_behind, self.max_stale_ms, self.max_kept
        )
    }
}

/// cladefs: keeps what `bucket` did, its requests by kind and its bytes up and down
/// (`bucket_cell::object_bucket::Stats::line`), what the stores' followers are
/// ([`FollowerStats`]), what serving the stores cost ([`daemon_line`]), the requests served
/// (`super::metrics::requests_line`), what its uploads hold ([`uploads_line`]) and what its process
/// holds and spent (`super::process::line`, sampled every poll, as the metrics report it, file or
/// not), while it lives: in the file `CLADEFSD_STATS` names, if it names one, a line each, rewritten whole within [`STATS_POLL`] of each change, which the
/// bench reports (`scripts/bench-report.py`); and the bucket's in the log every [`STATS_EVERY`]
/// that it changed. cladefs: what it keeps but the bucket's, it looks at only to keep it in the file
/// (TASK-92.17).
fn watch_stats(bucket: std::sync::Weak<bucket_cell::object_bucket::ObjectBucket>) {
    let file = node_local::env::var_os("CLADEFSD_STATS").filter(|file| !file.is_empty());
    let file = file.map(PathBuf::from);
    let mut logged = Stats::default();
    let mut followers = FollowerStats::default();
    let mut written = Vec::new();
    let (mut at, mut at_process) = (std::time::Instant::now(), std::time::Instant::now());
    // cladefs: a thread of its own, or under madsim a task (`super::run`, doc-24); a turn every
    // STATS_POLL, the first one poll after it starts.
    let mut started = false;
    let watching = super::run::every("cladefsd stats", STATS_POLL, std::thread::sleep, move || {
        if !std::mem::replace(&mut started, true) {
            return true;
        }
        {
            let Some(bucket) = bucket.upgrade() else { return false };
            let stats = bucket.stats();
            // cladefs: what the process holds, against its budget, whether or not a file keeps it
            // (TASK-330.2).
            super::process::sample();
            if let Some(file) = &file {
                followers.update();
                let lines = [
                    stats.line(),
                    followers.line(),
                    daemon_line(),
                    super::metrics::requests_line(),
                    uploads_line(),
                    cache_line(),
                    buckets_line(&bucket),
                    super::disk::line(),
                    super::peers::line().unwrap_or_else(|| "peers: none".to_owned()),
                    super::process::line(),
                ];
                // cladefs: the process's line, its CPU time changing each poll, every
                // PROCESS_EVERY at most, unless the rest changed (TASK-330.2).
                let (rest, process) = lines.split_at(lines.len() - 1);
                let changed = written.get(..rest.len()) != Some(rest)
                    || (written.last() != process.last() && at_process.elapsed() >= PROCESS_EVERY);
                if changed {
                    if let Err(err) = write_stats(file, &lines) {
                        log::warn!("cladefsd: writing {}: {err}", file.display());
                    }
                    written = lines.to_vec();
                    at_process = std::time::Instant::now();
                }
            }
            if stats != logged && at.elapsed() >= STATS_EVERY {
                log::info!("cladefsd: {}", stats.line());
                (logged, at) = (stats, std::time::Instant::now());
            }
        }
        true
    });
    if let Err(err) = watching {
        log::warn!("cladefsd: watching object storage's stats: {err}");
    }
}

/// cladefs: what the daemon's uploads hold of the objects they upload, in bytes, now and at most,
/// and the most they may (`crate::bucket::upload_memory`, TASK-317).
fn uploads_line() -> String {
    let memory = crate::bucket::upload_memory();
    format!("uploads: held={} max-held={} budget={}", memory.held, memory.max_held, memory.budget)
}

/// cladefs: the daemon's part cache, if it opened one ([`part_cache`]): opening none.
pub(super) fn opened_cache() -> Option<Arc<Cache>> {
    CACHE.get().lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone().flatten()
}

/// cladefs: what the daemon's part cache keeps, in bytes, and the most it may (TASK-406.1).
fn cache_line() -> String {
    match opened_cache() {
        Some(cache) => format!("cache: bytes={} capacity={}", cache.bytes(), cache.capacity()),
        None => "cache: none".to_owned(),
    }
}

/// cladefs: what serving the stores cost the daemon (`crate::stats::Stats::named`), as a line of
/// `NAME=COUNT` after `daemon:`.
fn daemon_line() -> String {
    let named = crate::stats::stats().named().map(|(name, count)| format!("{name}={count}"));
    // cladefs: and how long its stores' commits waited to be made durable (TASK-165.9): each
    // bucket of `local_cell::Lags` a count of its own, within its bound, the last beyond.
    let lags = local_cell::durability_lags();
    let quantile = |fraction| lags.quantile_ms(fraction).unwrap_or(0);
    let last = local_cell::LAG_BUCKETS - 1;
    let bucket = |(bucket, count): (usize, &u64)| match bucket == last {
        false => format!("local_lag_le_{}ms={count}", 1u64 << bucket),
        true => format!("local_lag_gt_{}ms={count}", 1u64 << (bucket - 1)),
    };
    let buckets: Vec<String> = lags.buckets.iter().enumerate().map(bucket).collect();
    // And where it went: how long its flushes and framings took, phase by phase.
    let timings = local_cell::timings();
    let timed = [
        timed_fields("flush", &timings.flushes, &local_cell::FLUSH_PHASES),
        timed_fields("framing", &timings.framings, &local_cell::FRAMING_PHASES),
        // cladefs: and making the segments framed durable, so many at a time (TASK-310).
        timed_fields("durable", &timings.durable, &local_cell::DURABLE_PHASES),
        vec![
            format!("durable_segments={}", timings.durable_segments),
            format!("own_sync_max_us={}", timings.own_sync_max_us),
            format!("raw_sync_max_us={}", timings.raw_sync_max_us),
        ],
    ]
    .concat();
    // cladefs: the bytes its stores wrote, by kind, against what the process wrote in all, as the
    // kernel counts it, and of that, what went before it was written back (TASK-310).
    // And the writes they took, each a page more than its bytes at most, as the kernel counts
    // them; and what the daemon wrote of its own files, this one's (TASK-330.2).
    let all = local_cell::written();
    let mut written: Vec<String> =
        all.named().map(|(kind, bytes)| format!("written_{kind}_bytes={bytes}")).into();
    written.push(format!("written_writes={}", all.writes));
    written.push(format!("stats_written_bytes={}", STATS_WRITTEN.load(Ordering::Relaxed)));
    // cladefs: and how often they wrote a segment's data once, framing it in place, and made a
    // commit spanning segments (TASK-310, TASK-92.41); and the bytes of segments' data their
    // framings read, and their looks ahead as the segments filled (TASK-400).
    let (framing_read, seen_ahead) = local_cell::framing_reads();
    let once = [
        format!("segments_framed_in_place={}", local_cell::segments_framed_in_place()),
        format!("commits_spanning_segments={}", local_cell::commits_spanning_segments()),
        format!("framing_read_bytes={framing_read}"),
        format!("seen_ahead_bytes={seen_ahead}"),
    ];
    // cladefs: and the parts their segments packed as they filled, held until their framings, and
    // those the governor let go of (TASK-400).
    let parts = super::memory::held_parts().counts();
    let held = [
        format!("held_parts_bytes={}", parts.bytes),
        format!("held_parts_peak_bytes={}", parts.peak),
        format!("held_parts_held_bytes={}", parts.held),
        format!("held_parts_taken_bytes={}", parts.taken),
        format!("held_parts_evicted_bytes={}", parts.evicted),
        format!("held_parts_evicted={}", parts.evicted_parts),
        format!("held_parts_late={}", parts.late_parts),
        format!("held_parts_skipped={}", parts.skipped_parts),
    ];
    let io = process_io().map(|(name, bytes)| format!("io_{name}={bytes}"));
    format!(
        "daemon: {} local_lag_count={} local_lag_max_us={} local_lag_p50_ms={} \
         local_lag_p99_ms={} local_lag_within_50ms={} {} {} {} {} {} {}",
        named.join(" "),
        lags.count,
        lags.max_us,
        quantile(0.5),
        quantile(0.99),
        lags.within_turn,
        buckets.join(" "),
        timed.join(" "),
        written.join(" "),
        once.join(" "),
        held.join(" "),
        io.join(" ")
    )
}

/// cladefs: what this process wrote, as the kernel counts it (/proc/self/io): the bytes it had
/// written to its disk, each as it dirtied the page, and of those, the bytes that went unwritten,
/// their file truncated or removed first; 0 where there is no /proc (TASK-310).
fn process_io() -> [(&'static str, u64); 2] {
    let io = host_fs::read_to_string("/proc/self/io").unwrap_or_default();
    let field = |name: &str| {
        (io.lines())
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(": "))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    };
    [
        ("write_bytes", field("write_bytes")),
        ("cancelled_write_bytes", field("cancelled_write_bytes")),
    ]
}

/// cladefs: `timed`, of `kind`, as `NAME=COUNT` fields: so many, so many slow, the slowest's
/// phases, by `names`, and each phase's longest (TASK-165.9).
fn timed_fields<const N: usize>(
    kind: &str,
    timed: &local_cell::Timed<N>,
    names: &[&str; N],
) -> Vec<String> {
    let mut fields =
        vec![format!("{kind}_count={}", timed.count), format!("{kind}_slow={}", timed.slow)];
    for (which, phases) in [("slowest", &timed.slowest), ("max", &timed.max)] {
        let named = names.iter().zip(phases.us);
        fields.extend(named.map(|(phase, us)| format!("{kind}_{which}_{phase}_us={us}")));
        // cladefs: and how long of it the threads waited to run (TASK-336).
        fields.push(format!("{kind}_{which}_runnable_us={}", phases.runnable_us));
    }
    // cladefs: and each phase summed over all of them, and their waits to run (TASK-336).
    let named = names.iter().zip(timed.sum.us);
    fields.extend(named.map(|(phase, us)| format!("{kind}_sum_{phase}_us={us}")));
    fields.push(format!("{kind}_sum_runnable_us={}", timed.sum.runnable_us));
    fields
}

/// cladefs: replaces `file` with `lines`, whole: a reader finds the old lines or the new. Counted,
/// in the pages it dirties ([`STATS_WRITTEN`]).
fn write_stats(file: &std::path::Path, lines: &[String]) -> std::io::Result<()> {
    let new = file.with_extension("new");
    let text = lines.join("\n") + "\n";
    host_fs::write(&new, &text)?;
    let pages = (text.len() as u64).div_ceil(crate::pages::page_size()) * crate::pages::page_size();
    STATS_WRITTEN.fetch_add(pages, Ordering::Relaxed);
    host_fs::rename(&new, file)
}

/// cladefs: the bytes of the pages the daemon's stats file took, written again and again
/// ([`write_stats`]): what its process's kernel counts it wrote of its own besides its stores',
/// its bucket's and its cache's, and its log's, which its checks of what it wrote allow
/// (`scripts/budgets.py`, TASK-330.2).
static STATS_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// cladefs: where the daemon keeps its part cache, and how much it may keep there, as the environment
/// says: the directory `CLADEFSD_PART_CACHE` names, if it names one (empty, it names none); the
/// bytes of objects `CLADEFSD_PART_CACHE_BYTES` gives, a budget, as a host whose cache shares its
/// disk with its stores needs (TASK-406.1), or, unset, none: as many as the disk governor leaves it
/// (doc-22). Read at the daemon's start, to refuse what it can't use, and again by its first store
/// to open.
pub fn cache_config() -> Result<Option<(std::path::PathBuf, u64)>, String> {
    let set = |var| node_local::env::var_os(var).filter(|value| !value.is_empty());
    let dir = set("CLADEFSD_PART_CACHE");
    let capacity = match set("CLADEFSD_PART_CACHE_BYTES") {
        None => u64::MAX,
        Some(_) if dir.is_none() => {
            return Err("CLADEFSD_PART_CACHE_BYTES is set, but CLADEFSD_PART_CACHE names no \
                        directory for the cache"
                .to_owned());
        }
        Some(bytes) => match bytes.to_str().and_then(|bytes| bytes.parse().ok()) {
            Some(0) | None => {
                return Err(format!(
                    "CLADEFSD_PART_CACHE_BYTES must be a positive number of bytes: {bytes:?}"
                ));
            }
            Some(bytes) => bytes,
        },
    };
    Ok(dir.map(|dir| (dir.into(), capacity)))
}

node_local::node_local! {
    /// cladefs: the daemon's part cache, once its first store opened it, if it keeps one
    /// ([`part_cache`]).
    static CACHE: std::sync::Mutex<Option<Option<Arc<Cache>>>> = std::sync::Mutex::new(None);
}

/// cladefs: the cache the environment configures ([`cache_config`]), if it does, where what the
/// stores opened read from their bucket is kept. Opened once, by the first store to open, those
/// opening meanwhile waiting for it: a cache holds its directory locked, and two opened at once, as
/// a daemon's VMs booting at once open their stores, failed the second (TASK-67). A failure is the
/// caller's, and the next tries again.
pub(super) fn part_cache() -> Result<Option<Arc<Cache>>, Errno> {
    let mut opened = CACHE.get().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cache) = &*opened {
        return Ok(cache.clone());
    }
    let config = cache_config().map_err(|err| {
        log::error!("cladefsd: {err}");
        Errno::EINVAL
    })?;
    let cache = match config {
        Some((dir, capacity)) => match Cache::open(&dir, capacity) {
            Ok(cache) => Some(Arc::new(cache)),
            Err(err) => {
                log::error!("cladefsd: CLADEFSD_PART_CACHE: {err}");
                return Err(Errno::EIO);
            }
        },
        None => None,
    };
    Ok(opened.insert(cache).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cladefs: the daemon's first stores, opened at once, share the one cache, which holds its
    /// directory locked: each found none yet and opened its own, and the second's lock failed, its
    /// store with EIO, as a daemon started again served its VMs booted again all at once
    /// (fxfs-guest-soak, TASK-67: a VM of five exited 1 at boot).
    #[test]
    fn stores_opened_at_once_share_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe { std::env::set_var("CLADEFSD_PART_CACHE", dir.path()) };
        let at_once = std::sync::Barrier::new(16);
        let caches: Vec<_> = std::thread::scope(|scope| {
            let opening: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        at_once.wait();
                        part_cache()
                    })
                })
                .collect();
            opening.into_iter().map(|opening| opening.join().unwrap()).collect()
        });
        let first = caches[0].clone().expect("a cache").expect("CLADEFSD_PART_CACHE's");
        for cache in caches {
            let cache = cache.expect("a cache").expect("CLADEFSD_PART_CACHE's");
            assert!(Arc::ptr_eq(&cache, &first), "two caches of one directory");
        }
        assert_eq!(first.capacity(), u64::MAX, "a cache with no budget");
    }

    /// TASK-406.1: the daemon's cache keeps no more than `CLADEFSD_PART_CACHE_BYTES`, as a host
    /// whose cache shares its disk with its stores needs.
    #[test]
    fn the_cache_takes_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in a process of its own, whose other threads don't read
        // the environment yet.
        unsafe {
            std::env::set_var("CLADEFSD_PART_CACHE", dir.path());
            std::env::set_var("CLADEFSD_PART_CACHE_BYTES", "268435456");
        }
        let cache = part_cache().expect("a cache").expect("CLADEFSD_PART_CACHE's");
        assert_eq!(cache.capacity(), 256 << 20);
    }

    /// TASK-406.1: a budget the cache can't take stops the daemon at its start, naming the
    /// variable: none, not a number, or with no cache to take it.
    #[test]
    fn budgets_the_cache_cant_take_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: as above.
        unsafe { std::env::remove_var("CLADEFSD_PART_CACHE") };
        // SAFETY: as above: no other thread of this test's process runs.
        let set = |bytes: &str| unsafe { std::env::set_var("CLADEFSD_PART_CACHE_BYTES", bytes) };
        set("1");
        assert_eq!(
            cache_config(),
            Err("CLADEFSD_PART_CACHE_BYTES is set, but CLADEFSD_PART_CACHE names no directory \
                 for the cache"
                .to_owned())
        );
        // SAFETY: as above.
        unsafe { std::env::set_var("CLADEFSD_PART_CACHE", dir.path()) };
        for bytes in ["0", "-1", "1G", "x"] {
            set(bytes);
            assert_eq!(
                cache_config(),
                Err(format!(
                    "CLADEFSD_PART_CACHE_BYTES must be a positive number of bytes: {bytes:?}"
                ))
            );
        }
        set("");
        assert_eq!(cache_config(), Ok(Some((dir.path().to_owned(), u64::MAX))));
    }
}
