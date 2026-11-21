// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A VM process's side of a connection: an extent store the daemon serves, and the cell's window
//! in the VM's memory.

pub(crate) use super::codec::Opening;
use super::codec::{
    Advanced, Area, Data, Encoded, Fault, Forked, Info, Made, Opened, PrivateCosts, ReadInto,
    Request, StageCounts, StageOp, Told, stream_bytes,
};
use super::wire::*;
use crate::dax::Faults;
use crate::dax::uffd::{FaultThread, UffdSecondStage};
use crate::error::{Errno, Error, Result};
use crate::hearing::{Latest, Occurrence};
use crate::{
    Access, CellVersion, Commit, ExtentId, ExtentInfo, MappingContents, Prefault, SecondStage,
    VolatileSlice, VolatileSliceMut, WindowMapping, WriteBarriers,
};
use bucket_cell::cell::Kind;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use std::borrow::Cow;
use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// An extent store that an extent daemon serves this process. See the module docs.
pub struct CellClient {
    client: Arc<Client>,
    cell: u64,
    /// The daemon's physical memory, which the window maps pages of, by arena: its
    /// tenant's, the default tenant's if of none, and the default arena, read-only (TASK-43,
    /// TASK-79.17).
    memories: Vec<(crate::pages::ArenaId, OwnedFd)>,
    /// The thread serving the guest's faults on the window, once it is created.
    faults: Mutex<Option<FaultThread>>,
    /// cladefs: whether it is a follower's, read-only (backlog doc-8).
    read_only: bool,
    /// Its transfer area, which reads and writes move their data through ([`TRANSFER`]).
    transfer: Arc<Transfer>,
    /// cladefs: whether its window copies on write (backlog doc-13), once created so.
    copying: std::sync::atomic::AtomicBool,
    /// cladefs: how many mappings the host lets a process make, if it limits them, as the daemon
    /// says ([`Opened`]), which its window budgets its own by: this process, in a jail without
    /// /proc, can't read it (TASK-172).
    max_map_count: Option<u64>,
}

impl CellClient {
    /// cladefs: the most bytes of a guest's stats an answer carries
    /// ([`CellClient::answer_stats`], TASK-111.4).
    pub const MAX_STATS: usize = MAX_STATS;

    /// The cell at `cell_dir` that the daemon listening at `socket` serves, created with `capacity`
    /// bytes if it doesn't exist ([`DaemonConnection::open`]).
    pub fn connect(
        socket: impl AsRef<Path>,
        cell_dir: impl AsRef<Path>,
        capacity: u64,
    ) -> Result<Self, Error> {
        DaemonConnection::connect(socket)?.open(cell_dir, capacity)
    }

    /// The cell at `cell_dir` that the daemon listening at `socket` serves, forked from the version
    /// `from` if it doesn't exist ([`DaemonConnection::open_forked`]).
    pub fn connect_forked(
        socket: impl AsRef<Path>,
        cell_dir: impl AsRef<Path>,
        from: CellVersion,
        capacity: u64,
    ) -> Result<Self, Error> {
        DaemonConnection::connect(socket)?.open_forked(cell_dir, from, capacity)
    }

    /// cladefs: a follower of the cell at `cell_dir` that the daemon listening at `socket` serves
    /// ([`DaemonConnection::open_read_only`]).
    pub fn connect_read_only(
        socket: impl AsRef<Path>,
        cell_dir: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        DaemonConnection::connect(socket)?.open_read_only(cell_dir)
    }

    /// The cell at `cell_dir`, served over `stream` ([`DaemonConnection::opening`]).
    #[cfg(test)]
    pub(crate) fn over(
        stream: UnixStream,
        cell_dir: &Path,
        opening: Opening,
    ) -> Result<Self, Error> {
        DaemonConnection::over(stream)?.opening(cell_dir, opening)
    }

    /// Opens the cell at `cell_dir` over `client`'s connection ([`DaemonConnection::opening`]): its
    /// `OPEN`, the VM process's build, and its transfer area.
    fn open(client: Arc<Client>, cell_dir: &Path, opening: Opening) -> Result<Self, Error> {
        let read_only = matches!(opening, Opening::ReadOnly);
        let open = Request::Open { path: cell_dir.to_owned(), opening };
        let reply = client.request_blocking(&open).map_err(|errno| match errno {
            Errno::EPROTONOSUPPORT => Error::new(anyhow::Error::new(errno).context(format!(
                "the extent daemon refused this VM process's wire version {VERSION}: it is of \
                 another, which its log names"
            ))),
            errno => Error::from(errno),
        })?;
        let opened = Opened::from_args(reply.header.args).map_err(|errno| match errno {
            Errno::EPROTONOSUPPORT => Error::new(anyhow::Error::new(errno).context(format!(
                "the extent daemon is of wire version {}, this VM process of {VERSION}",
                super::codec::version(&reply.header)
            ))),
            errno => Error::new(anyhow::Error::new(errno).context("the daemon named no arena")),
        })?;
        let Opened { cell, arena, commit, max_map_count } = opened;
        let mut fds = reply.fds.into_iter();
        let (Some(own), Some(public)) = (fds.next(), fds.next()) else {
            return Err(Error::msg("the daemon sent no physical memory"));
        };
        // Its tenant's arena, and the default one, read-only.
        let memories = vec![(arena, own), (crate::pages::ArenaId::DEFAULT, public)];
        // A follower's view's commit, which it has heard of.
        client.committed.rise(commit);
        // cladefs: its build, which the daemon's metrics show (TASK-111.7).
        client.request_blocking(&Request::Build(crate::BUILD_COMMIT)).map_err(|errno| {
            Error::new(anyhow::Error::new(errno).context("saying this VM process's build"))
        })?;
        let transfer = Transfer::give(&client)
            .map_err(|err| err.context("giving the daemon a transfer area"))?
            .into();
        let copying = std::sync::atomic::AtomicBool::new(false);
        let faults = Mutex::new(None);
        Ok(Self { client, cell, memories, faults, read_only, transfer, copying, max_map_count })
    }

    /// cladefs: whether it is a follower's, read-only ([`CellClient::connect_read_only`]).
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// cladefs: the number of the latest commit of a follower's writer it has heard of: its view's,
    /// or one after ([`CellClient::on_committed`]).
    pub fn committed(&self) -> u64 {
        self.client.committed.latest()
    }

    /// cladefs: calls `committed` with the number of each commit of a follower's writer it hears
    /// of from now on: the latest of each run of them, which the follower may advance to. Returns
    /// the latest heard of as it began: each commit is heard of from one or the other, or a later
    /// one, one as it begins perhaps from both; a second read, of
    /// [`CellClient::committed`], might miss one that the call misses (TASK-444.2.2). Once,
    /// of a cell.
    pub fn on_committed(&self, committed: impl Fn(u64) + Send + Sync + 'static) -> u64 {
        self.client.committed.listen(Box::new(committed))
    }

    /// cladefs: moves a follower's view to its writer's latest commit, holding what the commits
    /// since the one it was as of held until it lets go of them ([`CellClient::let_go`]),
    /// and returns the commit's number (`ADVANCE`). `ESTALE` once it was cut off.
    pub fn advance(&self) -> Result<u64> {
        blocking(self.advance_async())
    }

    /// [`CellClient::advance`], waiting for the daemon holding no thread.
    pub async fn advance_async(&self) -> Result<u64> {
        let reply = self.client.request(&Request::Advance).await?;
        Ok(Advanced::from_args(reply.header.args).commit)
    }

    /// cladefs: lets go of what a follower's view held of the commits before the one
    /// numbered `commit`, which an advance returned (`LET_GO`).
    pub fn let_go(&self, commit: u64) -> Result<()> {
        blocking(self.let_go_async(commit))
    }

    /// [`CellClient::let_go`], waiting for the daemon holding no thread.
    pub async fn let_go_async(&self, commit: u64) -> Result<()> {
        self.client.request(&Request::LetGo { commit }).await.map(drop)
    }

    /// cladefs: keeps `extent`, which a follower's view holds, whatever commit it advances to,
    /// until unpinned (`PIN`).
    pub fn pin(&self, extent: ExtentId) -> Result<()> {
        blocking(self.pin_async(extent))
    }

    /// [`CellClient::pin`], waiting for the daemon holding no thread.
    pub async fn pin_async(&self, extent: ExtentId) -> Result<()> {
        self.client.request(&Request::Pin(extent)).await.map(drop)
    }

    /// cladefs: lets go of `extent`, pinned (`UNPIN`).
    pub fn unpin(&self, extent: ExtentId) -> Result<()> {
        blocking(self.unpin_async(extent))
    }

    /// [`CellClient::unpin`], waiting for the daemon holding no thread.
    pub async fn unpin_async(&self, extent: ExtentId) -> Result<()> {
        self.client.request(&Request::Unpin(extent)).await.map(drop)
    }

    /// Calls `changed` with the cell's capacity each time the daemon resizes it (the control API's
    /// `ResizeCell`), and whether the guest is asked to shrink to it, which it answers
    /// ([`CellClient::resized`]). Unless something is called so, the guest refuses every shrink.
    pub fn on_capacity(&self, changed: impl Fn(u64, bool) + Send + Sync + 'static) {
        let _ = self.client.capacity.set(Box::new(changed));
    }

    /// Answers the daemon's asking the guest to shrink to `capacity` bytes: whether it took it.
    pub fn resized(&self, capacity: u64, took: bool) -> Result<()> {
        blocking(self.resized_async(capacity, took))
    }

    /// [`CellClient::resized`], waiting for the daemon holding no thread.
    pub async fn resized_async(&self, capacity: u64, took: bool) -> Result<()> {
        self.client.request(&Request::Resized { capacity, took }).await.map(drop)
    }

    /// cladefs: calls `asked` with each of the daemon's asks for this process's stats and its
    /// guest's (`STATS_ASK`, TASK-111.4): it asks the guest, if it has one that answers, and says
    /// so, the guest's answer then going to [`CellClient::answer_stats`]; otherwise the
    /// cell answers at once, with no guest's stats, as it does unless something is called so.
    /// Called on the thread receiving the daemon's messages, it must not wait.
    pub fn on_stats_asked(&self, asked: impl Fn(u64) -> bool + Send + Sync + 'static) {
        let _ = self.client.stats_asked.set(Box::new(asked));
    }

    /// cladefs: answers the daemon's ask `ask` for this process's stats ([`STATS`]), with the
    /// guest's, `guest`, its text, if it has a guest that answered: those asked before it too.
    pub fn answer_stats(&self, ask: u64, guest: Option<&str>) -> Result<()> {
        blocking(self.answer_stats_async(ask, guest))
    }

    /// [`CellClient::answer_stats`], waiting for the daemon holding no thread.
    pub async fn answer_stats_async(&self, ask: u64, guest: Option<&str>) -> Result<()> {
        self.client.answer_stats(ask, guest).await.map(drop)
    }

    /// The cell whose extents the guest mints.
    pub fn cell(&self) -> u64 {
        self.cell
    }

    /// The daemon's physical memory, which it handed this VM process as it connected,
    /// and which its window maps pages of: its tenant's arena of it (TASK-43), the default
    /// tenant's if it is of none (TASK-79.17).
    pub fn physical_memory(&self) -> std::os::fd::BorrowedFd<'_> {
        self.memories[0].1.as_fd()
    }

    /// The default arena of the daemon's physical memory, which public bases' pages are
    /// of, as every VM process holds it: read-only (TASK-43, TASK-79.17).
    pub fn public_memory(&self) -> std::os::fd::BorrowedFd<'_> {
        self.memories[1].1.as_fd()
    }

    /// What the cell holds (`INFO`).
    pub fn info(&self) -> Result<ExtentInfo> {
        blocking(self.info_async())
    }

    /// [`CellClient::info`], waiting for the daemon holding no thread.
    pub async fn info_async(&self) -> Result<ExtentInfo> {
        Ok(Info::from_args(self.client.request(&Request::Info).await?.header.args).0)
    }

    /// Reads into `buffers` the bytes at `offset` in `extent` (`READ`).
    pub fn read(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &mut [VolatileSliceMut],
    ) -> Result<()> {
        blocking(self.read_async(extent, offset, buffers))
    }

    /// [`CellClient::read`], waiting for the daemon holding no thread.
    pub async fn read_async(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &mut [VolatileSliceMut<'_>],
    ) -> Result<()> {
        let len: usize = buffers.iter().map(VolatileSliceMut::len).sum();
        let read = |into| Request::Read { extent, offset, len: len as u64, into };
        if let Some(slot) = Transfer::take(&self.transfer, len) {
            let into = ReadInto::Transfer { at: slot.offset() };
            self.client.request_keeping(&read(into), &slot).await?;
            let data = slot.bytes(len);
            let mut at = 0;
            for buffer in buffers {
                at += buffer.copy_from(&data[at..]);
            }
            return Ok(());
        }
        let reply = self.client.request(&read(ReadInto::Reply)).await?;
        if reply.payload.len() != len {
            return Err(Errno::EIO);
        }
        let mut at = 0;
        for buffer in buffers {
            at += buffer.copy_from(&reply.payload[at..]);
        }
        Ok(())
    }

    /// Writes the bytes of `buffers`, in order, at `offset` in `extent` (`WRITE`).
    pub fn write(&self, extent: ExtentId, offset: u64, buffers: &[VolatileSlice]) -> Result<()> {
        self.write_with_barriers(extent, offset, buffers, WriteBarriers::default())
    }

    /// Writes, ordered with `barriers`, in one round trip to the daemon
    /// ([`OpenCell::write_with_barriers`](crate::OpenCell::write_with_barriers)).
    pub fn write_with_barriers(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice],
        barriers: WriteBarriers<'_>,
    ) -> Result<()> {
        blocking(self.write_with_barriers_async(extent, offset, buffers, barriers))
    }

    /// [`CellClient::write_with_barriers`], waiting for the daemon holding no thread.
    pub async fn write_with_barriers_async(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
        barriers: WriteBarriers<'_>,
    ) -> Result<()> {
        let len = buffers.iter().map(VolatileSlice::len).sum();
        let gather = |data: &mut [u8]| {
            let mut at = 0;
            for buffer in buffers {
                at += buffer.copy_to(&mut data[at..]);
            }
        };
        let write = |data| Request::Write { extent, offset, data, barriers };
        if let Some(mut slot) = Transfer::take(&self.transfer, len) {
            gather(slot.bytes_mut(len));
            let data = Data::Transfer { at: slot.offset(), len: len as u64 };
            return self.client.request_keeping(&write(data), &slot).await.map(drop);
        }
        let mut data = vec![0; len];
        gather(&mut data);
        self.client.request(&write(Data::Inline(&data))).await.map(drop)
    }

    /// How many reads and writes moved their data through the transfer area.
    #[cfg(test)]
    pub(super) fn transfers(&self) -> u64 {
        self.transfer.used.load(Ordering::Relaxed)
    }

    /// The transfer area's slots, taken until dropped, which no read or write then has.
    #[cfg(test)]
    pub(super) fn take_transfer_slots(&self) -> Vec<impl Sized> {
        std::iter::from_fn(|| Transfer::take(&self.transfer, 1)).collect()
    }

    /// Sends `header`, `payload` and `fds`, a request, as they are, however malformed, and
    /// returns its reply.
    #[cfg(test)]
    pub(super) fn call(
        &self,
        header: Header,
        payload: &[u8],
        fds: &[std::os::fd::BorrowedFd<'_>],
    ) -> std::result::Result<Message, Errno> {
        blocking(self.client.call(header, &[payload], fds, None))
    }

    /// Frees `extent` (`FREE`).
    pub fn free(&self, extent: ExtentId) -> Result<()> {
        blocking(self.free_async(extent))
    }

    /// [`CellClient::free`], waiting for the daemon holding no thread.
    pub async fn free_async(&self, extent: ExtentId) -> Result<()> {
        self.client.request(&Request::Free(extent)).await.map(drop)
    }

    /// cladefs: frees `extents`, each as [`CellClient::free`] does, in order: the first that
    /// fails fails the call, those before it freed. In as few requests as their payloads hold
    /// (`FREE_MANY`), a flush's dead extents in one round trip, rather than one each
    /// (TASK-92.13).
    pub fn free_many(&self, extents: &[ExtentId]) -> Result<()> {
        blocking(self.free_many_async(extents))
    }

    /// [`CellClient::free_many`], waiting for the daemon holding no thread.
    pub async fn free_many_async(&self, extents: &[ExtentId]) -> Result<()> {
        // Each request names its first extent in its arguments, and those after it its payload.
        let most = 1 + MAX_PAYLOAD as usize / EXTENT_ID_BYTES;
        for extents in extents.chunks(most) {
            let free = match extents {
                [first] => Request::Free(*first),
                [first, rest @ ..] => {
                    Request::FreeMany { first: *first, rest: Cow::Borrowed(rest) }
                }
                [] => unreachable!("chunks aren't empty"),
            };
            self.client.request(&free).await?;
        }
        Ok(())
    }

    /// Commits, and sets the root to `root` if given, uploaded (`COMMIT` with `COMMIT_UPLOADED`).
    pub fn commit(&self, root: Option<&[u8]>) -> Result<()> {
        blocking(self.commit_async(root, Commit::Uploaded))
    }

    /// Commits, and sets the root to `root` if given, once ordered, not local: the daemon makes
    /// it local soon after (`COMMIT`).
    pub fn commit_ordered(&self, root: Option<&[u8]>) -> Result<()> {
        blocking(self.commit_async(root, Commit::Ordered))
    }

    /// Commits, and sets the root to `root` if given, completing as `commit` says, waiting for
    /// the daemon holding no thread ([`CellClient::commit`],
    /// [`CellClient::commit_ordered`]).
    pub async fn commit_async(&self, root: Option<&[u8]>, commit: Commit) -> Result<()> {
        self.client.request(&Request::Commit { commit, root }).await.map(drop)
    }

    /// The root as of the last commit (`ROOT`).
    pub fn root(&self) -> Result<Vec<u8>> {
        blocking(self.root_async())
    }

    /// [`CellClient::root`], waiting for the daemon holding no thread.
    pub async fn root_async(&self) -> Result<Vec<u8>> {
        Ok(self.client.request(&Request::Root).await?.payload)
    }

    /// Forks the cell, as of its last commit made uploaded, to a new cell, whose handle is a new
    /// directory at `path`, which the daemon serves sharing the cell's pages (`FORK`), and returns
    /// the cell's ID, the bucket's to give (TASK-364).
    pub fn fork(&self, path: &Path) -> Result<u64> {
        let fork = Request::Fork { path: path.to_owned() };
        let reply = self.client.request_blocking(&fork)?;
        Ok(Forked::from_args(reply.header.args).cell)
    }

    /// Makes a version of the cell, once nothing is open
    /// ([`OpenCell::is_quiescent`](crate::OpenCell::is_quiescent)), as with its guest's
    /// filesystem frozen, `EBUSY` otherwise: what a VM snapshot holds, which any number of hosts
    /// fork at once ([`DaemonConnection::open_forked`]), and none writes.
    pub fn make_version(&self) -> Result<CellVersion> {
        let reply = self.client.request_blocking(&Request::Version)?;
        let Forked { cell, version, tenant } = Forked::from_args(reply.header.args);
        Ok(CellVersion { tenant, cell, version })
    }

    /// cladefs: drops the version `version`, which [`CellClient::make_version`] made on this
    /// connection, `EPERM` for any other: what a VM snapshot that failed past making it names
    /// nowhere, which its cell would keep for good otherwise (TASK-575).
    pub fn drop_version(&self, version: u64) -> Result<()> {
        self.client.request_blocking(&Request::DropVersion { version })?;
        Ok(())
    }

    /// The window's mappings, which a snapshot of the VM records (`MAPPINGS`).
    pub fn window_mappings(&self) -> Result<Vec<WindowMapping>> {
        let payload = self.client.request_blocking(&Request::Mappings)?.payload;
        // A reply the daemon malformed.
        super::codec::mappings(&payload).map_err(|_| Errno::EIO)
    }

    /// cladefs: maps `mappings`, a snapshot's
    /// [`window_mappings`](CellClient::window_mappings), again, each at the same offset in
    /// the window, its extent's bytes or zeros, in order, the first that fails failing the call,
    /// those before it mapped: in as few requests as their payloads hold (`MAP_MANY`), each one
    /// change of the window, rather than a `MAP` each, a round trip and a change apiece
    /// (TASK-92.14).
    pub fn map_many(&self, mappings: &[WindowMapping]) -> Result<()> {
        let most = (MAX_PAYLOAD / MAPPING_BYTES) as usize;
        for mappings in mappings.chunks(most) {
            let map = Request::MapMany(Cow::Borrowed(mappings));
            self.client.request_blocking(&map)?;
        }
        Ok(())
    }

    /// Maps `len` bytes at `offset` in `extent`, sealed, into the window at `window_offset`,
    /// read-only (`MAP`).
    pub fn map(&self, extent: ExtentId, offset: u64, len: u64, window_offset: u64) -> Result<()> {
        blocking(self.map_async(extent, offset, len, window_offset))
    }

    /// [`CellClient::map`], waiting for the daemon holding no thread.
    pub async fn map_async(
        &self,
        extent: ExtentId,
        offset: u64,
        len: u64,
        window_offset: u64,
    ) -> Result<()> {
        let contents = MappingContents::Extent { extent, offset };
        let map = Request::Map(WindowMapping { window_offset, len, contents });
        self.client.request(&map).await.map(drop)
    }

    /// cladefs: maps zeros, read-only, at `len` bytes of the window at `window_offset` (doc-4's
    /// `MAP_ZEROS`, backlog doc-13): a hole's, or what is past a file's end, whose write makes a
    /// private page of zeros, nothing copied.
    pub async fn map_zeros_async(&self, window_offset: u64, len: u64) -> Result<()> {
        let zeros = WindowMapping { window_offset, len, contents: MappingContents::Zeros };
        self.client.request(&Request::Map(zeros)).await.map(drop)
    }

    /// cladefs: makes `len` bytes of the window at `window_offset` private now, as writes to each
    /// of their pages would, mapped writable (doc-4's `PRIVATE`, backlog doc-13): `ENOMEM` past its
    /// private pages, none made private; `EINVAL` unless the window copies on write.
    pub fn make_private(&self, window_offset: u64, len: u64) -> Result<()> {
        blocking(self.make_private_async(window_offset, len))
    }

    /// [`CellClient::make_private`], waiting for the daemon holding no thread.
    pub async fn make_private_async(&self, window_offset: u64, len: u64) -> Result<()> {
        let start = std::time::Instant::now();
        let made = self.client.request(&Request::Private { window_offset, len }).await.map(drop);
        // cladefs: as this process waited for it, the daemon's and its stage operations' parts
        // apart (TASK-164.10).
        self.client.stage_counts.private_request_since(start);
        made
    }

    /// cladefs: the runs of its window's private pages, by offset, in order: none without a
    /// window that copies on write (TASK-456.3).
    pub fn private_runs(&self) -> Result<Vec<std::ops::Range<u64>>> {
        match self.client.stage.get() {
            Some(stage) => stage.private_runs(),
            None => Ok(Vec::new()),
        }
    }

    /// cladefs: whether its window copies on write, a writer's
    /// ([`create_dax_window_in_host_memory`](Self::create_dax_window_in_host_memory)).
    pub fn copies_on_write(&self) -> bool {
        self.copying.load(Ordering::Acquire)
    }

    /// cladefs: maps the pages of the window's `len` bytes at `window_offset` for the guest ahead
    /// of its accesses: the pages a guest maps read-only to read next (doc-4's `MAP_PREFAULT`),
    /// installed in the VM process's memory, then mapped for the guest through the window's
    /// `Prefault`, which the window leaves to the guest's first touches otherwise (TASK-112.1,
    /// TASK-276.6). Without a window, nothing.
    pub fn prefault(&self, window_offset: u64, len: u64) {
        if let Some(stage) = self.client.stage.get() {
            stage.prefault(window_offset, len);
        }
    }

    /// Unmaps `len` bytes of the window at `window_offset` (`UNMAP`).
    pub fn unmap(&self, window_offset: u64, len: u64) -> Result<()> {
        blocking(self.unmap_async(window_offset, len))
    }

    /// [`CellClient::unmap`], waiting for the daemon holding no thread.
    pub async fn unmap_async(&self, window_offset: u64, len: u64) -> Result<()> {
        self.client.request(&Request::Unmap { window_offset, len }).await.map(drop)
    }

    /// cladefs: the stage operations this process made of the cell's window so far, and the
    /// nanoseconds they took, which it tells the daemon ([`STAGE_COUNTS`]).
    #[cfg(test)]
    pub(super) fn stage_counts(&self) -> StageCounts {
        self.client.stage_counts()
    }

    /// cladefs: the runs of private pages this process made of the cell's window so far, their
    /// pages and nanoseconds, which it tells the daemon ([`STAGE_PRIVATE_COUNTS`]).
    #[cfg(test)]
    pub(super) fn private_counts(&self) -> PrivateCosts {
        self.client.stage_counts.private()
    }

    /// cladefs: the runs of pages its window keeps installed at most, once it is created.
    #[cfg(test)]
    pub(super) fn window_budget(&self) -> Option<usize> {
        self.client.stage.get().map(|stage| stage.budget())
    }

    /// cladefs: whether its daemon went, closing the connection: what fails from then on fails for
    /// that (TASK-231).
    pub fn daemon_lost(&self) -> bool {
        self.client.lost.happened()
    }

    /// Has `lost` called once the daemon goes, closing the connection: at once if it went already.
    /// Nothing the cell asks is served from then on, its window's faults included, and it doesn't
    /// connect again: a VMM stops its VM, which can't go on, visibly to its host (TASK-50.2).
    /// [`DaemonConnection::on_lost`] says it before the cell opens, so that a daemon going as it
    /// opens is told of too (TASK-232). Called once, of the cell and its connection.
    pub fn on_daemon_lost(&self, lost: impl Fn() + Send + Sync + 'static) {
        self.client.lost.listen(Box::new(lost));
    }

    /// Creates the cell's window in the `size` bytes of this process's memory at `base`, which a
    /// hypervisor such as KVM maps as guest memory: the daemon maps its pages there, read-only, and
    /// the guest's faults there go to it. `prefault`, if any, is told of the pages mapped for the
    /// guest ahead of its accesses. cladefs: a writer's copies on write (backlog doc-13), of
    /// `private_max` private pages at most, one at least, this process's memory, for which the
    /// daemon reserves nothing (TASK-501): a write to a page, by the guest's own threads
    /// ([`crate::guest_thread_started`]), or one of a page mapped here, makes the page private. A
    /// follower's copies nothing, refusing every write, its `private_max` 0 (`EINVAL` otherwise, as
    /// a writer's of 0).
    ///
    /// # Safety
    ///
    /// The memory must be a page-aligned mapping that nothing else in the process uses, and that
    /// outlives the cell.
    pub unsafe fn create_dax_window_in_host_memory(
        &self,
        base: NonNull<u8>,
        size: u64,
        prefault: Option<Arc<dyn Prefault>>,
        private_max: u64,
    ) -> Result<(), Error> {
        // SAFETY: As the caller promises.
        unsafe { self.create_window(base, size, prefault, private_max, None) }
    }

    /// cladefs: creates the cell's window as [`Self::create_dax_window_in_host_memory`] does, a
    /// writer's, copying on write, of as many private pages as it has, over a VM's memory, whose
    /// faults on a page the daemon may not have mapped yet have `unmapped` map it first: a memory
    /// cell's (TASK-456.3), whose window nothing else maps.
    ///
    /// # Safety
    ///
    /// As [`Self::create_dax_window_in_host_memory`]'s.
    pub unsafe fn create_memory_window_in_host_memory(
        &self,
        base: NonNull<u8>,
        size: u64,
        prefault: Option<Arc<dyn Prefault>>,
        unmapped: Arc<dyn Unmapped>,
    ) -> Result<(), Error> {
        let pages = size / crate::pages::page_size();
        // SAFETY: As the caller promises.
        unsafe { self.create_window(base, size, prefault, pages, Some(unmapped)) }
    }

    /// The window of [`Self::create_dax_window_in_host_memory`], its faults on what isn't mapped
    /// yet first given to `unmapped`, if any.
    ///
    /// # Safety
    ///
    /// As [`Self::create_dax_window_in_host_memory`]'s.
    unsafe fn create_window(
        &self,
        base: NonNull<u8>,
        size: u64,
        prefault: Option<Arc<dyn Prefault>>,
        private_max: u64,
        unmapped: Option<Arc<dyn Unmapped>>,
    ) -> Result<(), Error> {
        let private_max = (private_max != 0).then_some(private_max);
        if private_max.is_some() == self.read_only {
            return Err(Error::new(Errno::EINVAL));
        }
        let mut faults = self.faults.lock().unwrap();
        if faults.is_some() {
            return Err(Error::new(Errno::EEXIST));
        }
        let memories = (self.memories.iter())
            .map(|(arena, fd)| Ok((*arena, fd.as_fd().try_clone_to_owned()?)))
            .collect::<std::io::Result<_>>()
            .map_err(Error::new)?;
        let phys = crate::dax::uffd::Memories::Given(memories);
        // SAFETY: As the caller promises.
        let stage = unsafe {
            UffdSecondStage::new(base.as_ptr() as usize, size, phys, prefault, self.max_map_count)
        }?;
        if private_max.is_some() {
            stage.copy_on_write()?;
        }
        // cladefs: a VM's memory's window, every write a guest's (TASK-456.3).
        if unmapped.is_some() {
            stage.writes_are_guests();
        }
        let _ = self.client.stage.set(stage.clone());
        // cladefs: and the runs of pages it keeps installed, which the daemon's metrics show
        // (TASK-111.5).
        let budget = stage.budget() as u64;
        self.client.request_blocking(&Request::Window { size, private_max, budget })?;
        self.copying.store(private_max.is_some(), Ordering::Release);
        let faults_of = DaemonFaults { client: self.client.clone(), size, unmapped };
        *faults = Some(stage.serve(Arc::new(faults_of))?);
        Ok(())
    }
}

impl Drop for CellClient {
    fn drop(&mut self) {
        // cladefs: the window lets go of the daemon's pages first, which the daemon frees once the
        // connection goes, for other VMs' extents, and makes none of the changes the daemon sent
        // before (TASK-79.8): the guest reaches nothing there until another connection's window
        // takes the memory over.
        if let Some(stage) = self.client.stage.get() {
            stage.release();
        }
        // The faults stop being served: the daemon stops serving them once disconnected.
        self.faults.get_mut().unwrap().take();
        // cladefs: what the window's changes cost, all of it, which the daemon counts (TASK-91.16).
        self.client.tell_stage_counts();
        self.client.close();
    }
}

/// cladefs: a VM process's connection to its extent daemon, which opens a cell over it, once: a
/// [`CellClient`]. What the VM process does once the daemon goes is said of it before the
/// cell opens ([`DaemonConnection::on_lost`]), so that a daemon going as it opens the cell, as
/// one killed while the cell recovers from its bucket does, is told of as one going later is,
/// rather than as the open's failure (TASK-232).
pub struct DaemonConnection {
    /// Its client, until a cell opened over it takes it.
    client: Option<Arc<Client>>,
}

impl DaemonConnection {
    /// A connection to the daemon listening at `socket`.
    pub fn connect(socket: impl AsRef<Path>) -> Result<Self, Error> {
        let stream = UnixStream::connect(socket.as_ref()).map_err(|err| {
            Error::new(anyhow::Error::new(err).context(format!(
                "connecting to the extent daemon at {}",
                socket.as_ref().display()
            )))
        })?;
        Self::over(stream)
    }

    /// A connection to the daemon at the other end of `stream`.
    pub(crate) fn over(stream: UnixStream) -> Result<Self, Error> {
        Ok(Self { client: Some(Client::start(stream).map_err(Error::new)?) })
    }

    /// Has `lost` called once the daemon goes, closing the connection: at once if it went already;
    /// before what was asked of it then fails, a cell's open included. Nothing is served from then
    /// on, a cell's window's faults included, and it doesn't connect again: a VMM stops its VM,
    /// which can't go on, visibly to its host (TASK-50.2), as it creates the VM too (TASK-232).
    /// Called once, of the connection and its cell ([`CellClient::on_daemon_lost`]).
    pub fn on_lost(&self, lost: impl Fn() + Send + Sync + 'static) {
        self.client().lost.listen(Box::new(lost));
    }

    /// The cell at `cell_dir`, created with `capacity` bytes if it doesn't exist.
    pub fn open(self, cell_dir: impl AsRef<Path>, capacity: u64) -> Result<CellClient, Error> {
        self.opening(cell_dir.as_ref(), Opening::Create(capacity, Kind::Fxfs))
    }

    /// cladefs: the cell at `cell_dir`, as [`DaemonConnection::open`] has it, made, if it doesn't
    /// exist, a SQLite cell, which the sqlite_cladefs VFS alone opens, rather
    /// than an Fxfs one (TASK-364).
    pub fn open_sqlite(
        self,
        cell_dir: impl AsRef<Path>,
        capacity: u64,
    ) -> Result<CellClient, Error> {
        self.opening(cell_dir.as_ref(), Opening::Create(capacity, Kind::Sqlite))
    }

    /// cladefs: the cell at `cell_dir`, as [`DaemonConnection::open`] has it, made, if it doesn't
    /// exist, a memory cell, a VM's memory (TASK-456.2), rather than an Fxfs one.
    pub fn open_memory(
        self,
        cell_dir: impl AsRef<Path>,
        capacity: u64,
    ) -> Result<CellClient, Error> {
        self.opening(cell_dir.as_ref(), Opening::Create(capacity, Kind::Memory))
    }

    /// The cell at `cell_dir`, of `capacity` bytes, forked from the version `from` if it doesn't
    /// exist: of a VM restored from a snapshot (TASK-39.1), on any host. cladefs: the daemon opens
    /// the version once for the restores of one snapshot here, and keeps it open for them
    /// (TASK-92.5).
    pub fn open_forked(
        self,
        cell_dir: impl AsRef<Path>,
        from: CellVersion,
        capacity: u64,
    ) -> Result<CellClient, Error> {
        let CellVersion { tenant, cell, version } = from;
        self.opening(cell_dir.as_ref(), Opening::ForkOf { tenant, cell, version, capacity })
    }

    /// cladefs: a follower of the cell at `cell_dir` (backlog doc-8): read-only, as of the latest
    /// commit of its writer, another VM's, which goes on, or of the cell as of its last commit, if
    /// no VM writes it; and which it moves to the writer's latest as it advances
    /// ([`CellClient::advance`]).
    pub fn open_read_only(self, cell_dir: impl AsRef<Path>) -> Result<CellClient, Error> {
        self.opening(cell_dir.as_ref(), Opening::ReadOnly)
    }

    /// The cell at `cell_dir`, opened as `opening` says. cladefs: by a daemon of this VM process's
    /// wire version alone, which takes every request there is (TASK-141.3): one of another, or
    /// that refuses any of what opening asks, the connection is closed on, failing; and one that
    /// goes as the cell opens fails it saying so (TASK-232).
    pub(crate) fn opening(
        mut self,
        cell_dir: &Path,
        opening: Opening,
    ) -> Result<CellClient, Error> {
        let client = self.client.take().expect("a connection opens one cell");
        CellClient::open(client.clone(), cell_dir, opening).map_err(|err| {
            let err = match client.lost.happened() {
                true => err.context("the extent daemon went"),
                false => err,
            };
            client.close();
            err.context(format!("opening the extent store at {}", cell_dir.display()))
        })
    }

    fn client(&self) -> &Client {
        self.client.as_ref().expect("a connection opens one cell")
    }
}

impl Drop for DaemonConnection {
    /// Closes it, if no cell was opened over it.
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            client.close();
        }
    }
}

/// A reply, or the error the daemon failed the request with.
type Reply = std::result::Result<Message, Errno>;

thread_local! {
    /// Whether this thread receives a daemon's replies ([`Client::receive`]).
    static RECEIVING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Waits for `future`, a cell's request, holding this thread. On a thread receiving a daemon's
/// replies, as a future [`spawn_on_replies`] runs, it would wait for itself: no such path is left
/// (the VMM's requests are made with `_async` methods, its callbacks make none), but one that
/// came to be fails its request rather than the VM process (`EIO`, TASK-56).
fn blocking<T, E: From<Errno>>(
    future: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> std::result::Result<T, E> {
    if RECEIVING.get() {
        log::error!(
            "a cell's request waited for on a thread receiving replies: {}",
            std::backtrace::Backtrace::force_capture()
        );
        return Err(Errno::EIO.into());
    }
    // cladefs: outside the simulation: the sync twins of the client's `_async` calls, for sync
    // callers, tools' and tests', which wait here, an outermost entry's (board 2948); the VMM's
    // backend calls the `_async` ones (board 5614).
    futures::executor::block_on(future)
}

/// Runs `future`, which makes requests of cells daemons serve (their `_async` methods), on this
/// thread until it waits for a reply, and from then on on the thread receiving each reply it
/// waits for, as it arrives: what follows a reply runs with no thread's wake-up between them
/// (TASK-41). So it must not block, as a cell's blocking calls would, nor take long: the replies
/// and window changes behind it wait.
pub fn spawn_on_replies(future: impl std::future::Future<Output = ()> + Send + 'static) {
    // Run as scheduled, by the thread that wakes it; woken while running, by the thread running
    // it, once it has polled it.
    let (runnable, task) = async_task::spawn(future, |runnable: async_task::Runnable| {
        runnable.run();
    });
    task.detach();
    runnable.run();
}

/// The connection, its requests waiting for replies, and the window's second stage, which the
/// thread receiving from the daemon makes its changes to.
struct Client {
    sender: Sender,
    next: AtomicU64,
    /// The requests waiting for replies; `None` once the connection is gone.
    waiting: Mutex<Option<Waiting>>,
    /// cladefs: signaled as replies come, making room for requests ([`Waiting::has_room`]), to
    /// those waiting for it ([`Waiting::for_room`]).
    room: std::sync::Condvar,
    stage: OnceLock<Arc<UffdSecondStage>>,
    /// cladefs: what making the stage operations cost this process, which it tells the daemon
    /// (TASK-91.16).
    stage_counts: StageCosts,
    /// What the daemon's changes of the cell's capacity go to
    /// ([`CellClient::on_capacity`]).
    capacity: OnceLock<Box<dyn Fn(u64, bool) + Send + Sync>>,
    /// cladefs: what the daemon's asks for stats go to ([`CellClient::on_stats_asked`]).
    stats_asked: OnceLock<Box<dyn Fn(u64) -> bool + Send + Sync>>,
    /// cladefs: the latest commit of a follower's writer heard of, and what hears of the next
    /// ([`CellClient::on_committed`]).
    committed: Latest,
    /// Whether the VM process is closing the connection itself ([`Client::close`]).
    closing: std::sync::atomic::AtomicBool,
    /// Whether the daemon went, closing the connection, and what is told of it
    /// ([`DaemonConnection::on_lost`], TASK-50.2).
    lost: Occurrence,
}

impl Client {
    /// A client of the daemon at the other end of `stream`, receiving on a thread of its own.
    fn start(stream: UnixStream) -> std::io::Result<Arc<Self>> {
        let client = Arc::new(Self {
            sender: Sender::new(stream.try_clone()?),
            next: AtomicU64::new(1),
            waiting: Mutex::new(Some(Waiting::default())),
            room: std::sync::Condvar::new(),
            stage: OnceLock::new(),
            stage_counts: StageCosts::default(),
            capacity: OnceLock::new(),
            stats_asked: OnceLock::new(),
            committed: Latest::new(),
            closing: Default::default(),
            lost: Occurrence::new(),
        });
        let receiving = client.clone();
        // cladefs: outside the simulation: a VM process's client's receiving thread.
        crate::thread::spawn("cladefsd client", move || receiving.receive(Receiver::new(stream)))?;
        Ok(client)
    }

    /// Closes the connection itself: its end is no daemon's going.
    fn close(&self) {
        self.closing.store(true, Ordering::Release);
        self.sender.shutdown();
    }

    /// Sends `request`, encoded ([`Request::encode`]), and returns its reply.
    fn request(&self, request: &Request<'_>) -> BoxFuture<'static, Reply> {
        self.request_with(request, None)
    }

    /// [`Client::request`], for a request whose data is in the transfer area's `slot`, which it
    /// keeps from other requests until the daemon answers, as until then the daemon may access it:
    /// the slot taken until both it and the request's caller are done with it, though the caller
    /// drops its request first (TASK-444.15).
    fn request_keeping(&self, request: &Request<'_>, slot: &Slot) -> BoxFuture<'static, Reply> {
        self.request_with(request, Some(slot.0.clone()))
    }

    fn request_with(&self, request: &Request<'_>, kept: Option<Kept>) -> BoxFuture<'static, Reply> {
        let Encoded { header, payload, fd } = request.encode();
        let payload = payload.each_ref().map(|part| &**part);
        self.call(header, &payload, fd.as_slice(), kept)
    }

    fn request_blocking(&self, request: &Request<'_>) -> Reply {
        blocking(self.request(request))
    }

    /// Sends `header`, `payload`'s parts and `fds`, a request, and returns its reply: once the
    /// protocol's bounds of what is in flight leave room for it, waiting for replies to make it,
    /// but on the thread receiving them (TASK-79.16). `kept` it holds until the daemon answers
    /// ([`Client::request_keeping`]).
    fn call(
        &self,
        mut header: Header,
        payload: &[&[u8]],
        fds: &[std::os::fd::BorrowedFd<'_>],
        kept: Option<Kept>,
    ) -> BoxFuture<'static, Reply> {
        header.id = self.next.fetch_add(1, Ordering::Relaxed);
        // cladefs: a call for the guest request current says it, first in its payload, which the
        // daemon serves the call with (`super::codec::guest_request`, TASK-260).
        let guest = crate::request::current_guest().map(super::codec::request_bytes);
        let with_guest: Vec<&[u8]>;
        let payload = match &guest {
            Some(guest) => {
                header.flags |= REQUEST;
                with_guest = std::iter::once(&guest[..]).chain(payload.iter().copied()).collect();
                &with_guest[..]
            }
            None => payload,
        };
        header.len = payload.iter().map(|part| part.len() as u64).sum();
        let bytes = stream_bytes(&header);
        let (reply, replied) = oneshot::channel();
        {
            let mut waiting = self.waiting.lock().unwrap();
            while !RECEIVING.get()
                && let Some(full) = waiting.as_mut().filter(|w| !w.has_room(bytes))
            {
                full.for_room += 1;
                waiting = self.room.wait(waiting).unwrap();
                // Unless the connection went meanwhile, and what waited with it.
                if let Some(waiting) = waiting.as_mut() {
                    waiting.for_room -= 1;
                }
            }
            match waiting.as_mut() {
                Some(waiting) => waiting.insert(header.id, reply, bytes, kept),
                None => return Box::pin(std::future::ready(Err(Errno::EIO))),
            }
        }
        if let Err(err) = self.sender.send_parts(header, payload, fds) {
            log::error!("sending to the extent daemon: {err}");
            self.answered(header.id);
            return Box::pin(std::future::ready(Err(Errno::EIO)));
        }
        Box::pin(async move { replied.await.unwrap_or(Err(Errno::EIO)) })
    }

    /// Takes the request `id` from those waiting, as it is answered, making room for others: those
    /// waiting for room told, if any are, but no system call made otherwise, as with none, the
    /// reply of every request.
    fn answered(&self, id: u64) -> Option<oneshot::Sender<Reply>> {
        let (answered, told) = {
            let mut waiting = self.waiting.lock().unwrap();
            let waiting = waiting.as_mut();
            let told = waiting.as_ref().is_some_and(|w| w.for_room > 0);
            (waiting.and_then(|w| w.remove(id)), told)
        };
        if told {
            self.room.notify_all();
        }
        answered
    }

    /// Receives from the daemon until it closes the connection: makes the stage operations, in
    /// order, and hands replies to their requests. cladefs: it tells the daemon what they cost,
    /// with each acknowledgement, and otherwise once [`STAGE_COUNTS_EVERY`] has passed since it
    /// last did, as it makes them (TASK-91.16).
    fn receive(&self, mut receiver: Receiver) {
        RECEIVING.set(true);
        // What a reply to MAPPINGS holds at most, past a message's most otherwise: a mapping a page
        // of its window (TASK-79.43).
        let mappings = || {
            let pages =
                self.stage.get().map_or(0, |stage| stage.size() / crate::pages::page_size());
            pages * MAPPING_BYTES
        };
        let mut told = std::time::Instant::now();
        let mut told_private = told;
        loop {
            let message = match receiver.receive(mappings) {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(err) => {
                    log::error!("receiving from the extent daemon: {err}");
                    break;
                }
            };
            let header = message.header;
            if header.kind == REPLY {
                let reply = match header.status {
                    0 => Ok(message),
                    status => Err(Errno::from_raw(-status)),
                };
                if let Some(waiting) = self.answered(header.id) {
                    let _ = waiting.send(reply);
                }
                continue;
            }
            // cladefs: as the codec has it, which refuses what the daemon gives no meaning
            // (TASK-160.9).
            let mut stage = match Told::decode(&header) {
                Ok(Told::Stage(stage)) => stage,
                Ok(Told::Capacity { capacity, ask }) => {
                    match self.capacity.get() {
                        Some(changed) => changed(capacity, ask),
                        // No guest to ask: it refuses.
                        None if ask => {
                            drop(self.request(&Request::Resized { capacity, took: false }));
                        }
                        None => {}
                    }
                    continue;
                }
                Ok(Told::Committed { commit }) => {
                    self.committed.rise(commit);
                    continue;
                }
                // cladefs: no guest asked, it answers with none (TASK-111.4).
                Ok(Told::Stats { ask }) => {
                    if !self.stats_asked.get().is_some_and(|asked| asked(ask)) {
                        drop(self.answer_stats(ask, None));
                    }
                    continue;
                }
                Err(errno) => {
                    log::error!("the extent daemon's message refused ({errno:?}): {header:?}");
                    break;
                }
            };
            let taken = match stage.op {
                StageOp::Private { .. } => receiver.take_privates(&mut stage).map(Taken::Privates),
                _ => receiver.take_change(&mut stage).map(Taken::Anew),
            };
            let taken = match taken {
                Ok(taken) => taken,
                Err(err) => {
                    log::error!("receiving a change to the window: {err}");
                    break;
                }
            };
            let start = std::time::Instant::now();
            let made = self.make(stage.op, &taken);
            self.stage_counts.count_since(start, &stage.op, &taken);
            if let Err(err) = made {
                log::error!("making the extent daemon's change to the window: {err}");
                break;
            }
            let tell = if stage.op.taking() {
                Made::Ack { id: stage.id, counts: self.stage_counts() }
            } else if told.elapsed() >= STAGE_COUNTS_EVERY {
                Made::Counts(self.stage_counts())
            } else {
                continue;
            };
            if let Err(err) = self.sender.send(tell.encode(), &[], &[]) {
                log::error!("telling the extent daemon of the window's changes made: {err}");
                break;
            }
            told = std::time::Instant::now();
            // cladefs: the runs of private pages, said as often, apart (TASK-164.10).
            if told.duration_since(told_private) >= STAGE_COUNTS_EVERY {
                let private = self.stage_counts.private();
                if (private[0] | private[3]) != 0
                    && let Err(err) =
                        self.sender.send(Made::PrivateCounts(private).encode(), &[], &[])
                {
                    log::error!("telling the extent daemon of the private pages made: {err}");
                    break;
                }
                told_private = told;
            }
        }
        // The daemon went, unless the VM process closed the connection itself: no request is
        // served, nor the window's faults, from now on, and the VM can't go on. Said before the
        // requests waiting fail, so that what their failures do knows why, as the window's faults
        // do, which wait for the VM process to stop then rather than abort it (TASK-231); and told
        // of then too, so that a VMM stops its VM for it, rather than for what fails, a cell's
        // open as its VM is created among them (TASK-232).
        if !self.closing.load(Ordering::Acquire) {
            log::error!("the extent daemon went: the cell's requests fail from now on");
            self.lost.happen();
        }
        // The requests waiting fail, as do those made from now on.
        self.waiting.lock().unwrap().take();
        self.room.notify_all();
        // Where a test has what the failing requests do come before the rest of the connection's
        // end (TASK-231, TASK-232).
        fail::fail_point!("client::requests_failed");
    }

    /// cladefs: tells the daemon what the stage operations made so far cost, if any were made
    /// ([`STAGE_COUNTS`]), and its runs of private pages, if any ([`STAGE_PRIVATE_COUNTS`]).
    fn tell_stage_counts(&self) {
        let counts = self.stage_counts();
        if counts.costs[0] == 0 {
            return;
        }
        let private = self.stage_counts.private();
        let told =
            (self.sender.send(Made::Counts(counts).encode(), &[], &[])).and_then(
                |()| match private[0] {
                    0 => Ok(()),
                    _ => self.sender.send(Made::PrivateCounts(private).encode(), &[], &[]),
                },
            );
        // A daemon gone counts nothing more.
        if let Err(err) = told
            && !self.lost.happened()
        {
            log::warn!("telling the extent daemon what the window's changes cost: {err}");
        }
    }

    /// cladefs: what the stage operations made so far cost, as [`STAGE_COUNTS`] says it: how many,
    /// the nanoseconds they took, and the changes among them that took access away from pages
    /// installed, by how (`dax::uffd::Taken`); and the runs of pages installed now (TASK-111.5).
    fn stage_counts(&self) -> StageCounts {
        let [ops, ns] = self.stage_counts.counts();
        let stage = self.stage.get();
        let [overwritten, left_out] = stage.map_or([0; 2], |stage| stage.taken());
        let installed = stage.map_or(0, |stage| stage.installed());
        StageCounts { costs: [ops, ns, overwritten, left_out], installed }
    }

    /// cladefs: answers the daemon's ask `ask` for stats ([`STATS`]), with the guest's, `guest`, if
    /// any: its stage counts said first ([`STAGE_COUNTS`]), so that the daemon has them as of the
    /// answer, which it receives after them (TASK-111.4).
    fn answer_stats(&self, ask: u64, guest: Option<&str>) -> BoxFuture<'static, Reply> {
        let counts = Made::Counts(self.stage_counts()).encode();
        if let Err(err) = self.sender.send(counts, &[], &[]) {
            log::warn!("telling the extent daemon the window's counts: {err}");
        }
        self.request(&Request::Stats { ask, guest })
    }

    /// Makes the stage operation `op` to the window, with what was taken after it, at once: an
    /// unmap with what follows it within what it unmaps, as [`Receiver::take_change`] takes them
    /// (TASK-91.16); cladefs: a private page with those following it on, the run
    /// [`Receiver::take_privates`] takes (TASK-164.10).
    fn make(&self, op: StageOp, taken: &Taken) -> std::io::Result<()> {
        let stage = self.stage.get().ok_or_else(|| std::io::Error::other("no window"))?;
        match (op, taken) {
            (StageOp::Unmap { offset, len }, Taken::Anew(anew)) if !anew.is_empty() => {
                stage.remap(offset, len, anew)
            }
            (StageOp::Map { offset, phys, len }, _) => stage.map(offset, phys, len),
            (StageOp::Unmap { offset, len }, _) => stage.unmap(offset, len),
            (StageOp::Mapped { offset, len }, _) => stage.window_mapped(offset, len),
            // cladefs: refused, as the daemon's error, of a window that doesn't copy.
            (StageOp::Private { .. }, _) if !stage.copying() => {
                return Err(std::io::Error::other("a private page of a window that doesn't copy"));
            }
            (StageOp::Private { offset, .. }, Taken::Privates(run)) => {
                stage.make_private(offset, run)
            }
            (StageOp::Private { offset, from }, Taken::Anew(_)) => {
                stage.make_private(offset, &[from])
            }
        }
        Ok(())
    }
}

/// The requests a client sent that wait for replies, by ID, with their bytes of stream data
/// ([`stream_bytes`]), and those bytes in all.
#[derive(Default)]
struct Waiting {
    /// Each request's: what its reply goes to, its stream bytes, and what it keeps until answered
    /// ([`Client::request_keeping`]), let go as it is taken from here.
    replies: HashMap<u64, (oneshot::Sender<Reply>, u64, Option<Kept>)>,
    stream_bytes: u64,
    /// cladefs: how many requests wait for room ([`Waiting::has_room`]).
    for_room: usize,
}

impl Waiting {
    /// cladefs: whether the protocol's bounds of what is in flight leave room for a request of
    /// `bytes` of stream data (TASK-79.16): any, with none in flight.
    fn has_room(&self, bytes: u64) -> bool {
        self.replies.is_empty()
            || (self.replies.len() < MAX_REQUESTS && self.stream_bytes + bytes <= MAX_STREAM_BYTES)
    }

    fn insert(&mut self, id: u64, reply: oneshot::Sender<Reply>, bytes: u64, kept: Option<Kept>) {
        self.replies.insert(id, (reply, bytes, kept));
        self.stream_bytes += bytes;
    }

    fn remove(&mut self, id: u64) -> Option<oneshot::Sender<Reply>> {
        let (reply, bytes, _kept) = self.replies.remove(&id)?;
        self.stream_bytes -= bytes;
        Some(reply)
    }
}

/// cladefs: what the receiving thread took with a stage operation, to make with it at once.
enum Taken {
    /// An unmap's maps anew within it ([`Receiver::take_change`]), none for another operation.
    Anew(Vec<crate::dax::Anew>),
    /// A private page's run, its own first ([`Receiver::take_privates`]).
    Privates(Vec<Option<crate::pages::PhysLocation>>),
}

/// cladefs: the stage operations a VM process made of a window, and the nanoseconds they took, as
/// [`STAGE_COUNTS`] tells them: the receiving thread counts them, which makes them. Of those, the
/// runs of private pages, the pages, and their nanoseconds (TASK-164.10).
#[derive(Default)]
struct StageCosts {
    ops: AtomicU64,
    ns: AtomicU64,
    private_runs: AtomicU64,
    private_pages: AtomicU64,
    private_ns: AtomicU64,
    /// The `PRIVATE` requests made, and the nanoseconds this process waited for their replies.
    private_requests: AtomicU64,
    private_request_ns: AtomicU64,
}

impl StageCosts {
    /// Counts an operation, `op` with what was `taken` with it, made since `start`.
    fn count_since(&self, start: std::time::Instant, op: &StageOp, taken: &Taken) {
        let ns = start.elapsed().as_nanos() as u64;
        self.ops.fetch_add(1, Ordering::Relaxed);
        self.ns.fetch_add(ns, Ordering::Relaxed);
        if let StageOp::Private { .. } = op {
            let pages = match taken {
                Taken::Privates(run) => run.len() as u64,
                Taken::Anew(_) => 1,
            };
            self.private_runs.fetch_add(1, Ordering::Relaxed);
            self.private_pages.fetch_add(pages, Ordering::Relaxed);
            self.private_ns.fetch_add(ns, Ordering::Relaxed);
        }
    }

    fn counts(&self) -> [u64; 2] {
        [self.ops.load(Ordering::Relaxed), self.ns.load(Ordering::Relaxed)]
    }

    /// Counts a `PRIVATE` request sent at `start`, its reply here.
    fn private_request_since(&self, start: std::time::Instant) {
        self.private_requests.fetch_add(1, Ordering::Relaxed);
        self.private_request_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// The runs of private pages, as [`STAGE_PRIVATE_COUNTS`] says them.
    fn private(&self) -> PrivateCosts {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        [
            load(&self.private_runs),
            load(&self.private_pages),
            load(&self.private_ns),
            load(&self.private_requests),
            load(&self.private_request_ns),
        ]
    }
}

/// A connection's transfer area ([`TRANSFER`]): shared memory the daemon maps too, in slots, each
/// a read's or a write's at a time, which moves its data there rather than through the socket
/// (TASK-26.18.6). A read or write larger than a slot, or that finds none free, goes through the
/// socket.
struct Transfer {
    addr: NonNull<u8>,
    /// The slots no read or write has.
    free: Mutex<Vec<usize>>,
    /// How many reads and writes took a slot.
    used: AtomicU64,
    /// cladefs: which slots this process wrote, each page, as it first took them
    /// ([`Transfer::take`]).
    written: [std::sync::atomic::AtomicBool; Transfer::SLOTS],
}

// SAFETY: Shared memory mapped for the area's life; each slot is one request's at a time.
unsafe impl Send for Transfer {}
// SAFETY: as Send's: threads sharing it take slots through `free`'s lock, and touch only their
// own slot's memory until they give it back.
unsafe impl Sync for Transfer {}

impl Transfer {
    /// The bytes of a slot: a request's data, as large as most of the guest's.
    const SLOT: usize = 256 << 10;
    /// The slots: as many as the daemon serves a connection's requests at once.
    const SLOTS: usize = 8;

    /// A transfer area, given to the daemon over `client`'s connection: the daemon's errno if it
    /// refuses it.
    fn give(client: &Client) -> Result<Self, Error> {
        let (transfer, file) = Self::make().map_err(Error::new)?;
        let area = Request::Transfer { len: transfer.len() as u64, area: Area(file.as_fd()) };
        client.request_blocking(&area)?;
        Ok(transfer)
    }

    /// The bytes of the area.
    fn len(&self) -> usize {
        Self::SLOT * Self::SLOTS
    }

    /// A transfer area, mapped, and its memory, sealed at its size.
    fn make() -> std::io::Result<(Self, std::fs::File)> {
        let len = Self::SLOT * Self::SLOTS;
        // SAFETY: The name is a C string.
        let fd = unsafe {
            libc::memfd_create(
                c"cladefs-transfer".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: Just created, and owned here.
        let fd = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let file = std::fs::File::from(fd);
        file.set_len(len as u64)?;
        // Sealed at its size: the daemon maps it only so, as it would fault past a new end.
        // cladefs: not against more seals, as the daemon seals it against writes once it mapped it
        // (TASK-547).
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
        // SAFETY: Seals the descriptor's memory, which this process made.
        if unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&file), libc::F_ADD_SEALS, seals) }
            != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: Maps the shared memory, of its size.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(&file),
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let transfer = Self {
            addr: NonNull::new(addr.cast()).unwrap(),
            free: Mutex::new((0..Self::SLOTS).rev().collect()),
            used: AtomicU64::new(0),
            written: Default::default(),
        };
        Ok((transfer, file))
    }

    /// A free slot of `transfer` for `len` bytes, if one is free and holds them.
    fn take(transfer: &Arc<Self>, len: usize) -> Option<Slot> {
        if len == 0 || len > Self::SLOT {
            return None;
        }
        let index = transfer.free.lock().unwrap().pop()?;
        transfer.used.fetch_add(1, Ordering::Relaxed);
        // cladefs: each page of it written here first, as this process's memory, which the
        // kernel charges to whoever faults it in: the daemon touches none it would fault
        // (TASK-547). As it is first taken, not as the area is made: a connection that moves no
        // data through it, as most forks' do, holds none of it. Written, not advised
        // (`MADV_POPULATE_WRITE`): a VM process's seccomp filter allows no more calls than it
        // needs.
        if !transfer.written[index].swap(true, Ordering::Relaxed) {
            for page in (0..Self::SLOT).step_by(crate::pages::page_size() as usize) {
                // SAFETY: Within the slot, which this thread alone has, of the area's mapping.
                unsafe { transfer.addr.as_ptr().add(index * Self::SLOT + page).write_volatile(0) };
            }
        }
        Some(Slot(Arc::new(TakenSlot { transfer: transfer.clone(), index })))
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        // SAFETY: Unmaps the area, which no slot is had of any more.
        unsafe { libc::munmap(self.addr.as_ptr().cast(), self.len()) };
    }
}

/// The bytes of a transfer area's slot, for tests.
#[cfg(test)]
pub(super) const TRANSFER_SLOT: usize = Transfer::SLOT;

/// The slots of a transfer area, for tests.
#[cfg(test)]
pub(super) const TRANSFER_SLOTS: usize = Transfer::SLOTS;

/// A slot of the transfer area, a request's: taken from the area's free slots while the request's
/// caller has it, and while the daemon may access it, until it answers the request
/// ([`Client::request_keeping`]).
struct Slot(Kept);

/// A slot taken, given back once neither the request's caller nor its waiting for the daemon's
/// answer has it.
type Kept = Arc<TakenSlot>;

/// A slot taken from its transfer area, given back as it is dropped.
struct TakenSlot {
    transfer: Arc<Transfer>,
    index: usize,
}

impl Slot {
    /// Where it is in the area.
    fn offset(&self) -> u64 {
        (self.0.index * Transfer::SLOT) as u64
    }

    /// Its first `len` bytes, which the daemon has left as they are: before the request, or once
    /// it replied.
    fn bytes(&self, len: usize) -> &[u8] {
        assert!(len <= Transfer::SLOT);
        // SAFETY: within the area, mapped while `self.0.transfer` is held, `len` bytes from the
        // slot's start, at most a slot's; no other request has the slot until it is given back,
        // when neither this nor the request's waiting holds it (`TakenSlot`'s drop), and the
        // daemon, which writes it in serving a read, has answered: the caller reads only once it
        // did.
        unsafe {
            std::slice::from_raw_parts(
                self.0.transfer.addr.as_ptr().add(self.offset() as usize),
                len,
            )
        }
    }

    /// Its first `len` bytes, to write, before the request is made.
    fn bytes_mut(&mut self, len: usize) -> &mut [u8] {
        assert!(len <= Transfer::SLOT);
        // SAFETY: as `bytes`'s, and before the request is made, the daemon has no access to it.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.0.transfer.addr.as_ptr().add(self.offset() as usize),
                len,
            )
        }
    }
}

impl Drop for TakenSlot {
    fn drop(&mut self) {
        self.transfer.free.lock().unwrap().push(self.index);
    }
}

/// The window's faults, which the daemon resolves.
struct DaemonFaults {
    client: Arc<Client>,
    size: u64,
    /// cladefs: what maps a page the daemon may not have mapped yet, before its fault goes to the
    /// daemon: a VM's memory's (TASK-456.3).
    unmapped: Option<Arc<dyn Unmapped>>,
}

/// cladefs: what maps a window's pages as faults first reach them, the daemon told nothing of
/// them before: a VM's memory cell's (TASK-456.3), whose window maps what its tree names of a
/// page only once the guest touches it.
pub trait Unmapped: Send + Sync {
    /// Maps, if it hasn't, what the window has at `offset`, a fault's page, resolving once the
    /// daemon has the mapping: the fault goes to the daemon then.
    fn reach(&self, offset: u64) -> BoxFuture<'static, Result<()>>;
}

impl DaemonFaults {
    fn fault_with(&self, fault: Fault) -> BoxFuture<'_, Result<()>> {
        let Some(unmapped) = &self.unmapped else {
            let reply = self.client.request(&Request::Fault(fault));
            return Box::pin(async move { reply.await.map(drop) });
        };
        let reached = unmapped.reach(fault.offset);
        Box::pin(async move {
            reached.await?;
            self.client.request(&Request::Fault(fault)).await.map(drop)
        })
    }
}

impl Faults for DaemonFaults {
    fn size(&self) -> u64 {
        self.size
    }

    fn fault(&self, offset: u64, access: Access) -> BoxFuture<'_, Result<()>> {
        self.fault_with(Fault { offset, access, page: false, probe: false })
    }

    fn fault_page(&self, offset: u64, access: Access) -> BoxFuture<'_, Result<()>> {
        self.fault_with(Fault { offset, access, page: true, probe: false })
    }

    fn probe(&self, offset: u64) -> BoxFuture<'_, Result<()>> {
        self.fault_with(Fault { offset, access: Access::Read, page: true, probe: true })
    }

    fn lost(&self) -> bool {
        self.client.lost.happened()
    }
}
