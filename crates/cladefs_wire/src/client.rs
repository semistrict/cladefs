// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A VM process's side of a connection: an extent store the daemon serves, and the cell's window
//! in the VM's memory.

pub use crate::codec::Opening;
use crate::codec::{
    Area, Data, Encoded, Fault, Forked, Info, Made, Opened, PrivateCosts, ReadInto, Request,
    StageCounts, StageOp, Told, stream_bytes,
};
use crate::wire::*;
use bucket_cell::cell::Kind;
#[cfg(target_os = "linux")]
use cladefs_core::Prefault;
use cladefs_core::dax::Faults;
#[cfg(target_os = "macos")]
use cladefs_core::dax::hvf::{
    FaultThread, GuestMemory, GuestRam, HvfRamStage, HvfSecondStage, HvfStage as WindowStage,
};
#[cfg(target_os = "linux")]
use cladefs_core::dax::uffd::{FaultThread, UffdSecondStage as WindowStage};
use cladefs_core::error::{Errno, Error, Result};
use cladefs_core::hearing::Occurrence;
use cladefs_core::{
    CellId, CellSnapshot, CellVersion, Commit, ExtentId, ExtentInfo, MappingContents, SecondStage,
    VolatileSlice, VolatileSliceMut, WindowMapping, WriteBarriers,
};
use futures::channel::oneshot;
use futures::future::BoxFuture;
use std::borrow::Cow;
use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// An extent store that an extent daemon serves this process. See the module docs.
pub struct CellClient {
    client: Arc<Client>,
    cell: u64,
    tenant: u64,
    ephemeral: bool,
    /// The daemon's physical memory, which the window maps pages of, by arena: its
    /// tenant's, the default tenant's if of none, and the default arena, read-only (TASK-43,
    /// TASK-79.17).
    memories: Vec<(cladefs_core::pages::ArenaId, OwnedFd)>,
    /// The thread serving the guest's faults on the window, once it is created.
    faults: Mutex<Option<FaultThread>>,
    /// cladefs: what resolves the window's faults through the daemon, once the window is created,
    /// which a `PRIVATE` reads its pages in with too ([`CellClient::make_private`], doc-26).
    window_faults: std::sync::OnceLock<Arc<DaemonFaults>>,
    /// Its transfer area, registered lazily on the first non-DAX read or write that fits a slot
    /// ([`TRANSFER`], board 11707, hotbed question 575).
    transfer: LazyTransfer,
    /// cladefs: whether its window copies on write (backlog doc-13), once created so.
    copying: std::sync::atomic::AtomicBool,
    /// cladefs: how many mappings the host lets a process make, if it limits them, as the daemon
    /// says ([`Opened`]), which its window budgets its own by, sharing them with this process's
    /// other windows, its other cells' (`dax::uffd::Mappings`, board 8493): this process, in a jail
    /// without /proc, can't read it (TASK-172).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "a window's: Linux's"))]
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

    /// Opens an existing managed cell `cell` served by the daemon listening at `socket`
    /// ([`DaemonConnection::open_cell`], hotbed question 547).
    pub fn connect_cell(
        socket: impl AsRef<Path>,
        cell: cladefs_core::CellId,
    ) -> Result<Self, Error> {
        DaemonConnection::connect(socket)?.open_cell(cell)
    }

    /// Creates and opens a new managed cell for `tenant` with `capacity` bytes on a disk chosen by
    /// the daemon listening at `socket` ([`DaemonConnection::create_cell`], hotbed question 547).
    pub fn connect_new(
        socket: impl AsRef<Path>,
        tenant: u64,
        capacity: u64,
    ) -> Result<Self, Error> {
        DaemonConnection::connect(socket)?.create_cell(tenant, capacity)
    }

    /// Forks version `from` into a new managed cell for `tenant` on a disk chosen by the daemon
    /// listening at `socket` ([`DaemonConnection::open_forked_cell`], hotbed question 547).
    pub fn connect_forked_cell(
        socket: impl AsRef<Path>,
        tenant: u64,
        from: CellVersion,
        capacity: u64,
    ) -> Result<Self, Error> {
        DaemonConnection::connect(socket)?.open_forked_cell(tenant, from, capacity)
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

    /// The cell at `cell_dir`, served over `stream` ([`DaemonConnection::opening`]).
    #[cfg(any(test, feature = "testing"))]
    pub fn over(stream: UnixStream, cell_dir: &Path, opening: Opening) -> Result<Self, Error> {
        DaemonConnection::over(stream)?.opening(cell_dir, opening)
    }

    /// Opens the cell at `cell_dir` over `client`'s connection ([`DaemonConnection::opening`]):
    /// `OPEN` (including the VM process's build). Its transfer area is registered lazily on the
    /// first non-DAX read or write (board 11707, hotbed question 575).
    fn open(client: Arc<Client>, cell_dir: &Path, opening: Opening) -> Result<Self, Error> {
        let open =
            Request::Open { path: cell_dir.to_owned(), opening, build: cladefs_core::BUILD_COMMIT };
        let reply = client.request_blocking(&open).map_err(|errno| match errno {
            Errno::EPROTONOSUPPORT => Error::new(anyhow::Error::new(errno).context(format!(
                "the extent daemon refused this VM process's wire version {VERSION}: it is of \
                 another, which its log names"
            ))),
            errno => Error::from(errno),
        })?;
        let opened =
            Opened::decode(&reply.header, &reply.payload).map_err(|errno| match errno {
                Errno::EPROTONOSUPPORT => Error::new(anyhow::Error::new(errno).context(format!(
                    "the extent daemon is of wire version {}, this VM process of {VERSION}",
                    crate::codec::version(&reply.header)
                ))),
                errno => Error::new(anyhow::Error::new(errno).context("the daemon named no arena")),
            })?;
        let Opened { cell, tenant, arena, max_map_count, ephemeral, capacity } = opened;
        // Preserve any non-ask `CAPACITY` notification received after the server published the
        // writer and before `OPEN`'s reply was decoded (board 11697).
        client.capacity_state.lock().unwrap().current.get_or_insert(capacity);
        let mut fds = reply.fds.into_iter();
        let (Some(own), Some(public)) = (fds.next(), fds.next()) else {
            return Err(Error::msg("the daemon sent no physical memory"));
        };
        // Its tenant's arena, and the default one, read-only.
        let memories = vec![(arena, own), (cladefs_core::pages::ArenaId::DEFAULT, public)];
        let transfer = LazyTransfer::new();
        let copying = std::sync::atomic::AtomicBool::new(false);
        let (faults, window_faults) = (Mutex::new(None), std::sync::OnceLock::new());
        Ok(Self {
            client,
            cell,
            tenant,
            ephemeral,
            memories,
            faults,
            window_faults,
            transfer,
            copying,
            max_map_count,
        })
    }

    /// Calls `changed` with the cell's capacity each time the daemon resizes it (the control API's
    /// `ResizeCell`), and whether the guest is asked to shrink to it, which it answers
    /// ([`CellClient::resized`]). Unless something is called so, the guest refuses every shrink.
    /// Once: a second would be called never, and panics.
    pub fn on_capacity(&self, changed: impl Fn(u64, bool) + Send + Sync + 'static) {
        assert!(self.client.capacity.set(Box::new(changed)).is_ok(), "on_capacity is called once");
    }

    /// Answers the daemon's asking the guest to shrink to `capacity` bytes: whether it took it.
    pub fn resized(&self, capacity: u64, took: bool) -> Result<()> {
        blocking(self.resized_async(capacity, took))
    }

    /// [`CellClient::resized`], waiting for the daemon holding no thread.
    pub async fn resized_async(&self, capacity: u64, took: bool) -> Result<()> {
        // Update the cached capacity before sending an accepted shrink answer: `Shrinking::resize`
        // holds its per-connection resize lock until our `RESIZED` arrives, so any subsequent
        // rollback (`Asked::drop` -> non-ask `CAPACITY(kept)`) or later resize notification is
        // causally ordered after this update (board 11697).
        {
            let mut state = self.client.capacity_state.lock().unwrap();
            if state.pending_shrink == Some(capacity) {
                state.pending_shrink = None;
                if took {
                    state.current = Some(capacity);
                }
            }
        }
        self.client.request(&Request::Resized { capacity, took }).await.map(drop)
    }

    /// cladefs: calls `asked` with each of the daemon's asks for this process's stats and its
    /// guest's (`STATS_ASK`, TASK-111.4): it asks the guest, if it has one that answers, and says
    /// so, the guest's answer then going to [`CellClient::answer_stats`]; otherwise the
    /// cell answers at once, with no guest's stats, as it does unless something is called so.
    /// Called on the thread receiving the daemon's messages, it must not wait.
    /// Once: a second would be called never, and panics.
    pub fn on_stats_asked(&self, asked: impl Fn(u64) -> bool + Send + Sync + 'static) {
        let set = self.client.stats_asked.set(Box::new(asked));
        assert!(set.is_ok(), "on_stats_asked is called once");
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

    /// cladefs: has the daemon serve the guest's doc-4 requests that come to `listener`, a
    /// listening Unix stream socket this process bound where its vsock device connects the guest's
    /// connections to the device's port ([`GUEST_STREAM`], board 14663): their uploaded commits
    /// answered once ordered if `ordered_only`. Once a connection.
    pub fn guest_stream(&self, ordered_only: bool, listener: BorrowedFd<'_>) -> Result<()> {
        let request = Request::GuestStream { ordered_only, listener: crate::codec::Area(listener) };
        let reply = self.client.request_blocking(&request)?;
        match reply.fds.is_empty() {
            true => Ok(()),
            // A daemon's reply of a descriptor is no reply this build's daemon sends.
            false => Err(Errno::EIO),
        }
    }

    /// The cell whose extents the guest mints.
    pub fn cell(&self) -> u64 {
        self.cell
    }

    /// The cell's tenant.
    pub fn tenant(&self) -> u64 {
        self.tenant
    }

    /// Whether the opened cell is a host-local ephemeral cell (doc-25).
    pub fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// The cell's current capacity in bytes, as reported in `OPEN`'s reply and updated by
    /// `CAPACITY` notifications and accepted shrinks (board 11697).
    pub fn capacity(&self) -> u64 {
        self.client.capacity_state.lock().unwrap().current.unwrap_or(0)
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
        let reply = self.client.request(&Request::Info).await?;
        Ok(Info::decode(&reply.header).map_err(|_| Errno::EIO)?.0)
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
        if (1..=Transfer::SLOT).contains(&len) {
            let transfer = self.transfer.get_or_init(&self.client).await?;
            if let Some(slot) = Transfer::take(&transfer, len) {
                let into = ReadInto::Transfer { at: slot.offset() };
                self.client.request_keeping(&read(into), &slot).await?;
                let data = slot.bytes(len);
                let mut at = 0;
                for buffer in buffers {
                    at += buffer.copy_from(&data[at..]);
                }
                return Ok(());
            }
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
        let len: usize = buffers.iter().map(VolatileSlice::len).sum();
        if (1..=Transfer::SLOT).contains(&len) {
            let transfer = self.transfer.get_or_init(&self.client).await?;
            if let Some(mut slot) = Transfer::take(&transfer, len) {
                cladefs_core::memory::gather(buffers, 0, slot.bytes_mut(len));
                let data = Data::Transfer { at: slot.offset(), len: len as u64 };
                let write = Request::Write { extent, offset, data, barriers };
                return self.client.request_keeping(&write, &slot).await.map(drop);
            }
        }
        // cladefs TASK-661: acquire request-credit backpressure before reading guest buffers, and
        // stream `buffers` through the sender's single bounded scratch buffer instead of gathering
        // a multi-MiB `Vec` per write on the VMM heap.
        let reply = self.client.write_inline(extent, offset, len as u64, buffers, barriers);
        reply.await.map(drop)
    }

    /// How many reads and writes moved their data through the transfer area.
    #[cfg(any(test, feature = "testing"))]
    pub fn transfers(&self) -> u64 {
        self.transfer.get().map_or(0, |transfer| transfer.used.load(Ordering::Relaxed))
    }

    /// The transfer area's slots, taken until dropped, which no read or write then has.
    #[cfg(any(test, feature = "testing"))]
    pub fn take_transfer_slots(&self) -> Vec<impl Sized> {
        let transfer = blocking(self.transfer.get_or_init(&self.client))
            .expect("taking transfer slots registers the transfer area");
        std::iter::from_fn(|| Transfer::take(&transfer, 1)).collect()
    }

    /// Sends `header`, `payload` and `fds`, a request, as they are, however malformed, and
    /// returns its reply.
    #[cfg(any(test, feature = "testing"))]
    pub fn call(
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
        let free = Request::Free { first: extent, rest: Cow::Borrowed(&[]) };
        self.client.request(&free).await.map(drop)
    }

    /// cladefs: frees `extents`, each as [`CellClient::free`] does, in order: the first that
    /// fails fails the call, those before it freed. In as few `FREE` requests as their payloads
    /// hold, a flush's dead extents in one round trip, rather than one each (TASK-92.13, board
    /// 11697).
    pub fn free_many(&self, extents: &[ExtentId]) -> Result<()> {
        blocking(self.free_many_async(extents))
    }

    /// [`CellClient::free_many`], waiting for the daemon holding no thread.
    pub async fn free_many_async(&self, extents: &[ExtentId]) -> Result<()> {
        // Each request names its first extent in its arguments, and those after it in its payload.
        let most = 1 + MAX_PAYLOAD as usize / EXTENT_ID_BYTES;
        for extents in extents.chunks(most) {
            let [first, rest @ ..] = extents else {
                unreachable!("chunks aren't empty");
            };
            let free = Request::Free { first: *first, rest: Cow::Borrowed(rest) };
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

    /// Snapshots the cell once nothing is open: makes a bucket-backed version for a regular cell,
    /// or quiesces and checkpoints a host-local ephemeral cell at its latest commit (`EBUSY` while
    /// an extent is open or the window holds private pages).
    pub fn snapshot_cell(&self) -> Result<CellSnapshot> {
        // cladefs: not while its window holds private pages, whose bytes no extent holds: a VM
        // restored from the snapshot would read their sources' old bytes (TASK-203). This process
        // alone knows of them (doc-26).
        if self.private_pages() > 0 {
            return Err(Errno::EBUSY);
        }
        let reply = self.client.request_blocking(&Request::Version)?;
        let Forked { version, tenant, committed, ephemeral } =
            Forked::decode(&reply.header).map_err(|_| Errno::EIO)?;
        if ephemeral {
            Ok(CellSnapshot::LocalEphemeral { tenant, cell: self.cell, committed })
        } else {
            Ok(CellSnapshot::Version(CellVersion { tenant, cell: self.cell, version }))
        }
    }

    /// Makes a version of the cell, once nothing is open
    /// ([`OpenCell::is_quiescent`](crate::OpenCell::is_quiescent)), as with its guest's
    /// filesystem frozen, `EBUSY` otherwise: what a VM snapshot holds, which any number of hosts
    /// fork at once ([`DaemonConnection::open_forked`]), and none writes. `ENOTSUP` for an
    /// ephemeral cell, which has no versions.
    pub fn make_version(&self) -> Result<CellVersion> {
        if self.ephemeral {
            return Err(Errno::ENOTSUP);
        }
        match self.snapshot_cell()? {
            CellSnapshot::Version(version) => Ok(version),
            CellSnapshot::LocalEphemeral { .. } => Err(Errno::ENOTSUP),
        }
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
        let payload =
            self.client.request_blocking(&Request::Mappings { quiescent: false })?.payload;
        // A reply the daemon malformed.
        crate::codec::mappings(&payload).map_err(|_| Errno::EIO)
    }

    /// The window's mappings, refusing with `EBUSY` if any extent is open (`!is_quiescent`):
    /// used when snapshotting or releasing a paused VM's cell for live migration without making a
    /// version.
    pub fn window_mappings_quiescent(&self) -> Result<Vec<WindowMapping>> {
        let payload = self.client.request_blocking(&Request::Mappings { quiescent: true })?.payload;
        // A reply the daemon malformed.
        crate::codec::mappings(&payload).map_err(|_| Errno::EIO)
    }

    /// cladefs: maps `mappings`, each at its offset in the window, its extent's bytes or zeros, in
    /// order, the first that fails failing the call, those before it mapped, in as few `MAP`
    /// requests as their payloads hold, each one change of the window (TASK-92.14, board 6917,
    /// board 11697).
    pub fn map_many(&self, mappings: &[WindowMapping]) -> Result<()> {
        blocking(self.map_many_async(mappings))
    }

    /// [`CellClient::map_many`], waiting for the daemon holding no thread.
    pub async fn map_many_async(&self, mappings: &[WindowMapping]) -> Result<()> {
        let most = (MAX_PAYLOAD / MAPPING_BYTES) as usize;
        for mappings in mappings.chunks(most) {
            let map = Request::Map { mappings: Cow::Borrowed(mappings), warm: false };
            self.client.request(&map).await?;
        }
        Ok(())
    }

    /// cladefs: [`CellClient::map_many`] with [`MAP_WARM`], warming each mapped extent range
    /// in the daemon's extent cache before installing the mappings so a following
    /// [`CellClient::prefault`] installs already-present pages without a throwaway `READ` IPC copy
    /// (board 11681, board 11697).
    pub fn map_many_warm(&self, mappings: &[WindowMapping]) -> Result<()> {
        blocking(self.map_many_warm_async(mappings))
    }

    /// [`CellClient::map_many_warm`], waiting for the daemon holding no thread.
    pub async fn map_many_warm_async(&self, mappings: &[WindowMapping]) -> Result<()> {
        let most = (MAX_PAYLOAD / MAPPING_BYTES) as usize;
        for mappings in mappings.chunks(most) {
            let map = Request::Map { mappings: Cow::Borrowed(mappings), warm: true };
            self.client.request(&map).await?;
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
        let mapping = WindowMapping { window_offset, len, contents };
        let map =
            Request::Map { mappings: Cow::Borrowed(std::slice::from_ref(&mapping)), warm: false };
        self.client.request(&map).await.map(drop)
    }

    /// cladefs: maps zeros, read-only, at `len` bytes of the window at `window_offset` (doc-4's
    /// `MAP_ZEROS`, backlog doc-13): a hole's, or what is past a file's end, whose write makes a
    /// private page of zeros, nothing copied.
    pub async fn map_zeros_async(&self, window_offset: u64, len: u64) -> Result<()> {
        let zeros = WindowMapping { window_offset, len, contents: MappingContents::Zeros };
        let map =
            Request::Map { mappings: Cow::Borrowed(std::slice::from_ref(&zeros)), warm: false };
        self.client.request(&map).await.map(drop)
    }

    /// cladefs: makes `len` bytes of the window at `window_offset` private now, as writes to each
    /// of their pages would, mapped writable (doc-4's `PRIVATE`, backlog doc-13): here, in this
    /// process's memory, the pages the window names nothing at read in through the daemon first
    /// (doc-26). `ENOMEM` past its private pages, none made private; `EINVAL` unless the window
    /// copies on write.
    pub fn make_private(&self, window_offset: u64, len: u64) -> Result<()> {
        blocking(self.make_private_async(window_offset, len))
    }

    /// [`CellClient::make_private`], waiting for the daemon holding no thread.
    pub async fn make_private_async(&self, window_offset: u64, len: u64) -> Result<()> {
        let (Some(stage), Some(faults)) = (self.client.stage.get(), self.window_faults.get())
        else {
            return Err(Errno::EINVAL);
        };
        let start = std::time::Instant::now();
        let made = stage.make_private(faults.as_ref(), window_offset, len).await;
        // cladefs: what serving it took (TASK-164.10).
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

    /// cladefs: how many of its window's pages are private now (backlog doc-13), this process's
    /// memory, which it alone knows of (doc-26): a snapshot of its VM is refused while any is.
    pub fn private_pages(&self) -> u64 {
        self.client.stage.get().map_or(0, |stage| stage.private_pages())
    }

    /// cladefs: whether it has a window, which copies on write
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
    #[cfg(any(test, feature = "testing"))]
    pub fn stage_counts(&self) -> StageCounts {
        self.client.stage_counts()
    }

    /// cladefs: the runs of private pages this process made of the cell's window so far, their
    /// pages and nanoseconds, which it tells the daemon ([`STAGE_COUNTS`]).
    #[cfg(any(test, feature = "testing"))]
    pub fn private_counts(&self) -> PrivateCosts {
        self.client.stage_counts.private(self.client.stage.get())
    }

    /// cladefs: the runs of pages its window keeps installed at most, once it is created.
    #[cfg(any(test, feature = "testing"))]
    pub fn window_budget(&self) -> Option<usize> {
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
    /// `private_max` private pages at most, one at least, this process's memory, which it bounds
    /// itself, and the daemon reserves nothing for (TASK-501, doc-26): a write to a page, by the
    /// guest's own threads ([`crate::guest_thread_started`]), or one of a page mapped here, makes
    /// the page private, a copy of what the daemon maps there. `EINVAL` of a `private_max` of 0.
    ///
    /// # Safety
    ///
    /// The memory must be a page-aligned mapping that nothing else in the process uses, and that
    /// outlives the cell.
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
    pub unsafe fn create_memory_window_in_host_memory(
        &self,
        base: NonNull<u8>,
        size: u64,
        prefault: Option<Arc<dyn Prefault>>,
        unmapped: Arc<dyn Unmapped>,
    ) -> Result<(), Error> {
        let pages = size / cladefs_core::pages::page_size();
        // SAFETY: As the caller promises.
        unsafe { self.create_window(base, size, prefault, pages, Some(unmapped)) }
    }

    /// The window of [`Self::create_dax_window_in_host_memory`], its faults on what isn't mapped
    /// yet first given to `unmapped`, if any.
    ///
    /// # Safety
    ///
    /// As [`Self::create_dax_window_in_host_memory`]'s.
    #[cfg(target_os = "linux")]
    unsafe fn create_window(
        &self,
        base: NonNull<u8>,
        size: u64,
        prefault: Option<Arc<dyn Prefault>>,
        private_max: u64,
        unmapped: Option<Arc<dyn Unmapped>>,
    ) -> Result<(), Error> {
        if private_max == 0 {
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
        let phys = cladefs_core::dax::uffd::Memories::Given(memories);
        // SAFETY: As the caller promises.
        let stage = unsafe {
            WindowStage::new(base.as_ptr() as usize, size, phys, prefault, self.max_map_count)
        }?;
        stage.copy_on_write(private_max)?;
        // cladefs: a VM's memory's window, every write a guest's (TASK-456.3).
        if unmapped.is_some() {
            stage.writes_are_guests();
        }
        // cladefs: one of an attempt that failed before holds it: refused, never served stale.
        self.client.stage.set(stage.clone()).map_err(|_| Error::new(Errno::EEXIST))?;
        // cladefs: and the runs of pages it keeps installed, which the daemon's metrics show
        // (TASK-111.5): its share of this process's windows' as it is made, which another window
        // coming or going changes (board 8493), said again then, below.
        let budget = stage.budget() as u64;
        self.client.request_blocking(&Request::Window { size, copying: true, budget })?;
        // cladefs: and again as another window of its process comes or goes, changing it, which
        // the daemon's metrics show as it is (board 10098): and once now, as it is now, which a
        // window that came and went since may have changed and changed back, the daemon keeping
        // WINDOW's, served apart, after a STAGE_COUNTS between (hotbed review 133).
        let client = Arc::downgrade(&self.client);
        stage.on_shared(move || {
            if let Some(client) = client.upgrade() {
                client.tell_budget();
            }
        });
        self.client.tell_budget();
        self.copying.store(true, Ordering::Release);
        let faults_of = Arc::new(DaemonFaults { client: self.client.clone(), size, unmapped });
        self.window_faults.set(faults_of.clone()).map_err(|_| Error::new(Errno::EEXIST))?;
        *faults = Some(stage.serve(faults_of)?);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl CellClient {
    /// Creates the cell's window in the guest memory of a VMM that maps it itself, as
    /// Hypervisor.framework's do (`dax::hvf`, board 13069): the daemon's pages are mapped into the
    /// guest through `guest` as the window maps them, read-only, and the guest's faults there,
    /// which the VMM hands to the stage returned ([`WindowStage::fault`]), go to the daemon. A
    /// writer's copies on write, of `private_max` private pages at most, as on Linux. `EINVAL` of
    /// a `private_max` of 0.
    pub fn create_dax_window_in_guest(
        &self,
        size: u64,
        guest: Arc<dyn GuestMemory>,
        private_max: u64,
    ) -> Result<Arc<HvfSecondStage>, Error> {
        if private_max == 0 {
            return Err(Error::new(Errno::EINVAL));
        }
        let stage = HvfSecondStage::new(size, self.memories()?, guest)?;
        stage.copy_on_write(private_max)?;
        self.serve_window(WindowStage::Window(stage.clone()))?;
        Ok(stage)
    }

    /// cladefs: creates the cell's window as the guest's RAM, `ram`, whose host memory the VMM's
    /// devices access as the guest does (`dax::hvf::ram`, board 13994): the daemon's pages mapped
    /// copy-on-write there as the window maps them, every write of the guest's or the VMM's a
    /// private copy, the kernel's. A memory cell's (TASK-456.3), whose window nothing else maps.
    pub fn create_memory_window_in_guest_ram(
        &self,
        ram: Arc<dyn GuestRam>,
    ) -> Result<Arc<HvfRamStage>, Error> {
        let stage = HvfRamStage::new(ram, self.memories()?)?;
        self.serve_window(WindowStage::Ram(stage.clone()))?;
        Ok(stage)
    }

    /// cladefs: the window's stage as the guest's RAM, once it is
    /// ([`Self::create_memory_window_in_guest_ram`]).
    pub fn guest_ram_stage(&self) -> Option<&Arc<HvfRamStage>> {
        match self.client.stage.get().map(|stage| &**stage) {
            Some(WindowStage::Ram(stage)) => Some(stage),
            _ => None,
        }
    }

    /// Faults the window's page at `offset` in through the daemon, as a guest's access would:
    /// its mapping comes back before the reply. `EINVAL` without a window.
    pub fn fault_in(&self, offset: u64) -> Result<()> {
        let faults = self.window_faults.get().ok_or(Errno::EINVAL)?;
        blocking(cladefs_core::dax::Faults::fault_page(faults.as_ref(), offset))
    }

    /// The daemon's memory objects, which the window maps pages of, for a stage of its own.
    fn memories(&self) -> Result<Vec<(cladefs_core::pages::ArenaId, OwnedFd)>, Error> {
        (self.memories.iter())
            .map(|(arena, fd)| Ok((*arena, fd.as_fd().try_clone_to_owned()?)))
            .collect::<std::io::Result<_>>()
            .map_err(Error::new)
    }

    /// Makes `stage` the cell's window, which the daemon is told of, its faults served.
    fn serve_window(&self, stage: WindowStage) -> Result<(), Error> {
        let mut faults = self.faults.lock().unwrap();
        if faults.is_some() {
            return Err(Error::new(Errno::EEXIST));
        }
        let stage = Arc::new(stage);
        let size = stage.size();
        self.client.stage.set(stage.clone()).map_err(|_| Error::new(Errno::EEXIST))?;
        let budget = stage.budget() as u64;
        self.client.request_blocking(&Request::Window { size, copying: true, budget })?;
        self.client.tell_budget();
        self.copying.store(true, Ordering::Release);
        let faults_of =
            Arc::new(DaemonFaults { client: self.client.clone(), size, unmapped: None });
        self.window_faults.set(faults_of.clone()).map_err(|_| Error::new(Errno::EEXIST))?;
        *faults = Some(match &*stage {
            WindowStage::Window(window) => window.serve(faults_of)?,
            // Its faults are the VMM's to serve, every page there before the guest runs.
            WindowStage::Ram(_) => FaultThread,
        });
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
    pub fn over(stream: UnixStream) -> Result<Self, Error> {
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

    /// The existing cell at `cell_dir` (`ENOENT` if it doesn't exist): used to reopen a released
    /// cell on a paused VM or open a cell handed over by live migration.
    pub fn open_existing(self, cell_dir: impl AsRef<Path>) -> Result<CellClient, Error> {
        self.opening(cell_dir.as_ref(), Opening::Existing)
    }

    /// The cell at `cell_dir`, created with `capacity` bytes if it doesn't exist.
    pub fn open(self, cell_dir: impl AsRef<Path>, capacity: u64) -> Result<CellClient, Error> {
        self.opening(cell_dir.as_ref(), Opening::Create(capacity, Kind::Fxfs))
    }

    /// Opens an existing managed cell `cell` by its `(tenant, cell)` ID without passing a host
    /// directory (hotbed question 547).
    pub fn open_cell(self, cell: cladefs_core::CellId) -> Result<CellClient, Error> {
        self.opening(Path::new(&cell.to_string()), Opening::Existing)
    }

    /// Creates and opens a new managed Fxfs cell for `tenant` with `capacity` bytes on a disk
    /// chosen automatically by the daemon (hotbed question 547).
    pub fn create_cell(self, tenant: u64, capacity: u64) -> Result<CellClient, Error> {
        let target = cladefs_core::CellId { tenant, id: 0 }.to_string();
        self.opening(Path::new(&target), Opening::Create(capacity, Kind::Fxfs))
    }

    /// Creates and opens a new managed memory cell for `tenant` with `capacity` bytes on a disk
    /// chosen automatically by the daemon (hotbed question 547).
    pub fn create_memory_cell(self, tenant: u64, capacity: u64) -> Result<CellClient, Error> {
        let target = cladefs_core::CellId { tenant, id: 0 }.to_string();
        self.opening(Path::new(&target), Opening::Create(capacity, Kind::Memory))
    }

    /// Forks version `from` into a new managed cell for `tenant` on a disk chosen automatically by
    /// the daemon (hotbed question 547).
    pub fn open_forked_cell(
        self,
        tenant: u64,
        from: CellVersion,
        capacity: u64,
    ) -> Result<CellClient, Error> {
        let target = cladefs_core::CellId { tenant, id: 0 }.to_string();
        let CellVersion { tenant: from_tenant, cell, version } = from;
        self.opening(
            Path::new(&target),
            Opening::ForkOf { tenant: from_tenant, cell, version, capacity },
        )
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

    /// cladefs: creates (or reopens) a standalone host-local ephemeral memory cell at `cell_dir`
    /// with `capacity` bytes (zero bucket uploads).
    pub fn open_memory_ephemeral(
        self,
        cell_dir: impl AsRef<Path>,
        capacity: u64,
    ) -> Result<CellClient, Error> {
        self.opening(cell_dir.as_ref(), Opening::CreateEphemeral(capacity, Kind::Memory))
    }

    /// cladefs: reopens an existing host-local ephemeral memory cell at `cell_dir` in place
    /// (`ENOENT` if missing, `EINVAL` if not an ephemeral memory cell, `EEXIST` if `expected` is
    /// given and differs from the cell on disk).
    pub fn open_ephemeral_memory_existing(
        self,
        cell_dir: impl AsRef<Path>,
        expected: Option<CellId>,
        capacity: u64,
    ) -> Result<CellClient, Error> {
        let (tenant, cell) = match expected {
            Some(CellId { tenant, id }) => (tenant, id),
            None => (0, 0),
        };
        self.opening(cell_dir.as_ref(), Opening::ExistingEphemeralMemory { tenant, cell, capacity })
    }

    /// cladefs: reopens an existing host-local ephemeral cell at `cell_dir` in place for a local
    /// VM snapshot restore, verifying its `(tenant, cell)` ID and latest commit `committed`
    /// (`ENOENT` if missing, `EINVAL` if not ephemeral, `EEXIST` if modified or replaced).
    pub fn open_ephemeral_at_commit(
        self,
        cell_dir: impl AsRef<Path>,
        cell: CellId,
        committed: u64,
        capacity: u64,
    ) -> Result<CellClient, Error> {
        self.opening(
            cell_dir.as_ref(),
            Opening::LocalEphemeral { tenant: cell.tenant, cell: cell.id, committed, capacity },
        )
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

    /// The cell at `cell_dir`, opened as `opening` says. cladefs: by a daemon of this VM process's
    /// wire version alone, which takes every request there is (TASK-141.3): one of another, or
    /// that refuses any of what opening asks, the connection is closed on, failing; and one that
    /// goes as the cell opens fails it saying so (TASK-232).
    pub fn opening(mut self, cell_dir: &Path, opening: Opening) -> Result<CellClient, Error> {
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
    // Per thread: whether it receives a client's replies, set for the thread's life; a client is no simulated host.
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

/// Tracks the cell's current capacity across `OPEN`, non-ask `CAPACITY` notifications, and
/// accepted shrink asks (`RESIZED` with `took == true`) without letting an `OPEN` reply overwrite
/// a newer concurrent `CAPACITY` notification (board 11697).
#[derive(Default)]
struct CapacityState {
    current: Option<u64>,
    pending_shrink: Option<u64>,
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
    stage: OnceLock<Arc<WindowStage>>,
    /// cladefs: what making the stage operations cost this process, which it tells the daemon
    /// (TASK-91.16).
    stage_counts: StageCosts,
    /// cladefs: held while a [`STAGE_COUNTS`] is made and sent, by whatever thread: the last sent
    /// says its window's budget as it was last set (board 10098).
    telling: Mutex<()>,
    /// The cell's current capacity and pending shrink ask (board 11697).
    capacity_state: Mutex<CapacityState>,
    /// What the daemon's changes of the cell's capacity go to
    /// ([`CellClient::on_capacity`]).
    capacity: OnceLock<Box<dyn Fn(u64, bool) + Send + Sync>>,
    /// cladefs: what the daemon's asks for stats go to ([`CellClient::on_stats_asked`]).
    stats_asked: OnceLock<Box<dyn Fn(u64) -> bool + Send + Sync>>,
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
            sender: Sender::new(stream.try_clone()?)?,
            next: AtomicU64::new(1),
            waiting: Mutex::new(Some(Waiting::default())),
            room: std::sync::Condvar::new(),
            stage: OnceLock::new(),
            stage_counts: StageCosts::default(),
            telling: Mutex::new(()),
            capacity_state: Mutex::new(CapacityState::default()),
            capacity: OnceLock::new(),
            stats_asked: OnceLock::new(),
            closing: Default::default(),
            lost: Occurrence::new(),
        });
        let receiving = client.clone();
        // cladefs: outside the simulation: a VM process's client's receiving thread.
        cladefs_core::thread::spawn("cladefsd client", move || {
            receiving.receive(Receiver::new(stream))
        })?;
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

    /// Sends an inline [`WRITE`] whose payload bytes live in `buffers`, acquiring request-credit
    /// backpressure before touching guest memory and streaming through [`Sender::send_stream`]
    /// without allocating a multi-MiB payload `Vec` (TASK-661).
    fn write_inline(
        &self,
        extent: ExtentId,
        offset: u64,
        data_len: u64,
        buffers: &[VolatileSlice<'_>],
        barriers: WriteBarriers<'_>,
    ) -> BoxFuture<'static, Reply> {
        let (header, root) = Request::write_inline_header(extent, offset, data_len, barriers);
        self.call_stream(header, &[], buffers, &[root], &[], None)
    }

    /// Sends `header`, `payload`'s parts and `fds`, a request, and returns its reply: once the
    /// protocol's bounds of what is in flight leave room for it, waiting for replies to make it,
    /// but on the thread receiving them (TASK-79.16). `kept` it holds until the daemon answers
    /// ([`Client::request_keeping`]).
    fn call(
        &self,
        header: Header,
        payload: &[&[u8]],
        fds: &[std::os::fd::BorrowedFd<'_>],
        kept: Option<Kept>,
    ) -> BoxFuture<'static, Reply> {
        self.call_stream(header, payload, &[], &[], fds, kept)
    }

    /// Sends `header` with `prefix`, volatile `buffers` (streamed after backpressure), `suffix`,
    /// and `fds`.
    fn call_stream(
        &self,
        mut header: Header,
        prefix: &[&[u8]],
        buffers: &[VolatileSlice<'_>],
        suffix: &[&[u8]],
        fds: &[std::os::fd::BorrowedFd<'_>],
        kept: Option<Kept>,
    ) -> BoxFuture<'static, Reply> {
        header.id = self.next.fetch_add(1, Ordering::Relaxed);
        // cladefs: a call for the guest request current says it, first in its payload, which the
        // daemon serves the call with (`super::codec::guest_request`, TASK-260).
        let guest = cladefs_core::request::current_guest().map(crate::codec::request_bytes);
        let with_guest: Vec<&[u8]>;
        let prefix = match &guest {
            Some(guest) => {
                header.flags |= REQUEST;
                with_guest = std::iter::once(&guest[..]).chain(prefix.iter().copied()).collect();
                &with_guest[..]
            }
            None => prefix,
        };
        let prefix_len: u64 = prefix.iter().map(|part| part.len() as u64).sum();
        let stream_len: u64 = buffers.iter().map(|buf| buf.len() as u64).sum();
        let suffix_len: u64 = suffix.iter().map(|part| part.len() as u64).sum();
        header.len = prefix_len + stream_len + suffix_len;
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
        if let Err(err) = self.sender.send_stream(header, prefix, buffers, suffix, fds) {
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
        // What a reply to MAPPINGS or an atomic STAGE_REMAP holds at most, past a message's most
        // otherwise: up to one mapping (or Mapped+Map pair) per logical block of its window
        // (TASK-79.43, board 11673).
        let mappings = || {
            let blocks =
                self.stage.get().map_or(0, |stage| stage.size() / cladefs_core::dax::map_unit());
            let per_block = MAPPING_BYTES.max(2 * size_of::<crate::wire::StageAnewEntry>() as u64);
            blocks * per_block
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
                    // Its waiter gone: nothing to tell.
                    let _ = waiting.send(reply);
                }
                continue;
            }
            // cladefs: as the codec has it, which refuses what the daemon gives no meaning
            // (TASK-160.9).
            let stage = match Told::decode(&header, &message.payload) {
                Ok(Told::Stage(stage)) => stage,
                Ok(Told::Capacity { capacity, ask }) => {
                    {
                        let mut state = self.capacity_state.lock().unwrap();
                        if ask {
                            state.pending_shrink = Some(capacity);
                        } else {
                            state.current = Some(capacity);
                            state.pending_shrink = None;
                        }
                    }
                    match self.capacity.get() {
                        Some(changed) => changed(capacity, ask),
                        // No guest to ask: it refuses.
                        None if ask => {
                            self.capacity_state.lock().unwrap().pending_shrink = None;
                            drop(self.request(&Request::Resized { capacity, took: false }));
                        }
                        None => {}
                    }
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
            let taking = stage.op.taking();
            let start = std::time::Instant::now();
            let made = self.make(stage.op);
            self.stage_counts.count_since(start);
            if let Err(err) = made {
                log::error!("making the extent daemon's change to the window: {err}");
                break;
            }
            let tell = if taking {
                let (header, payload) =
                    Made::Ack { id: stage.id, counts: self.stage_counts() }.encode();
                self.sender.send(header, &payload, &[])
            } else if told.elapsed() >= STAGE_COUNTS_EVERY {
                let sent = self.tell_counts();
                if sent.is_ok() {
                    told_private = std::time::Instant::now();
                }
                sent
            } else {
                continue;
            };
            if let Err(err) = tell {
                log::error!("telling the extent daemon of the window's changes made: {err}");
                break;
            }
            told = std::time::Instant::now();
            // cladefs: private page counts ride on `STAGE_COUNTS` too (TASK-164.10, board 11697):
            // if continuous `taking` acks kept `told` fresh, flush `STAGE_COUNTS` periodically once
            // any private pages or `PRIVATE` requests were recorded.
            if told.duration_since(told_private) >= STAGE_COUNTS_EVERY {
                let private = self.stage_counts.private(self.stage.get());
                if (private[0] | private[3]) != 0
                    && let Err(err) = self.tell_counts()
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
        // cladefs: and a send under way, waiting for room in the socket, fails too: its end shut
        // down here, as the daemon's shutting down its own fails no send on macOS (a BSD's), where
        // it waited for good, a daemon that let the VM process go reading nothing more (board
        // 13065). On Linux the daemon's shutdown failed it already.
        self.sender.shutdown();
        // Where a test has what the failing requests do come before the rest of the connection's
        // end (TASK-231, TASK-232).
        fail::fail_point!("client::requests_failed");
    }

    /// cladefs: tells the daemon what the stage operations and private-page runs made so far cost,
    /// if any were made ([`STAGE_COUNTS`]).
    fn tell_stage_counts(&self) {
        let private = self.stage_counts.private(self.stage.get());
        if self.stage_counts().costs[0] == 0 && (private[0] | private[3]) == 0 {
            return;
        }
        let told = self.tell_counts();
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
        let private = stage.map_or(0, |stage| stage.private_pages());
        let budget = stage.map_or(0, |stage| stage.budget() as u64);
        StageCounts { costs: [ops, ns, overwritten, left_out], installed, private, budget }
    }

    /// cladefs: tells the daemon what the window's second stage is now ([`STAGE_COUNTS`]), its
    /// budget and private-page costs among it: the last told, of every thread's, as it is last.
    fn tell_counts(&self) -> std::io::Result<()> {
        let _telling = self.telling.lock().unwrap();
        let (header, payload) = Made::Counts {
            stage: self.stage_counts(),
            private: self.stage_counts.private(self.stage.get()),
        }
        .encode();
        self.sender.send(header, &payload, &[])
    }

    /// cladefs: tells the daemon its window's budget as it is now, which another window of its
    /// process coming or going changed (board 10098).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "a window's: Linux's"))]
    fn tell_budget(&self) {
        // Closing, it tells nothing more: its socket is shut down (hotbed review 133).
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        // A daemon gone shows nothing more.
        if let Err(err) = self.tell_counts()
            && !self.lost.happened()
        {
            log::warn!("telling the extent daemon the window's budget: {err}");
        }
    }

    /// cladefs: answers the daemon's ask `ask` for stats ([`STATS`]), with the guest's, `guest`, if
    /// any: its stage counts said first ([`STAGE_COUNTS`]), so that the daemon has them as of the
    /// answer, which it receives after them (TASK-111.4).
    fn answer_stats(&self, ask: u64, guest: Option<&str>) -> BoxFuture<'static, Reply> {
        if let Err(err) = self.tell_counts() {
            log::warn!("telling the extent daemon the window's counts: {err}");
        }
        self.request(&Request::Stats { ask, guest })
    }

    /// Makes the stage operation `op` to the window at once: a [`StageOp::Remap`] unmaps and
    /// applies its `anew` entries atomically (TASK-91.16, board 11673); cladefs: keeping the
    /// private pages there, or ending them, as it says (doc-26).
    fn make(&self, op: StageOp) -> std::io::Result<()> {
        use cladefs_core::dax::Private;
        let stage = self.stage.get().ok_or_else(|| std::io::Error::other("no window"))?;
        let private = |keep| if keep { Private::Keep } else { Private::End };
        match op {
            StageOp::Remap { offset, len, keep_private, anew } => {
                stage.remap(offset, len, private(keep_private), &anew)
            }
            StageOp::Map { offset, phys, len } => stage.map(offset, phys, len),
            StageOp::Unmap { offset, len, keep_private } => {
                stage.unmap(offset, len, private(keep_private))
            }
            StageOp::Mapped { offset, len } => stage.window_mapped(offset, len),
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

/// cladefs: the stage operations a VM process made of a window, and the nanoseconds they took, as
/// [`STAGE_COUNTS`] tells them: the receiving thread counts them, which makes them. And the
/// guest's `PRIVATE`s it served (TASK-164.10, doc-26).
#[derive(Default)]
struct StageCosts {
    ops: AtomicU64,
    ns: AtomicU64,
    /// The `PRIVATE`s served, and the nanoseconds they took.
    private_requests: AtomicU64,
    private_request_ns: AtomicU64,
}

impl StageCosts {
    /// Counts an operation made since `start`.
    fn count_since(&self, start: std::time::Instant) {
        let ns = start.elapsed().as_nanos() as u64;
        self.ops.fetch_add(1, Ordering::Relaxed);
        self.ns.fetch_add(ns, Ordering::Relaxed);
    }

    fn counts(&self) -> [u64; 2] {
        [self.ops.load(Ordering::Relaxed), self.ns.load(Ordering::Relaxed)]
    }

    /// Counts a `PRIVATE` served from `start` on.
    fn private_request_since(&self, start: std::time::Instant) {
        self.private_requests.fetch_add(1, Ordering::Relaxed);
        self.private_request_ns.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// The runs of private pages `stage` made, if any, and the `PRIVATE`s served, as
    /// [`STAGE_COUNTS`]'s payload says them.
    fn private(&self, stage: Option<&Arc<WindowStage>>) -> PrivateCosts {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let [runs, pages, ns] = stage.map_or([0; 3], |stage| stage.private_costs());
        [runs, pages, ns, load(&self.private_requests), load(&self.private_request_ns)]
    }
}

/// Lazily creates and registers a connection's transfer area on the first non-DAX read or write
/// that fits a slot (board 11707, hotbed question 575). Idle and DAX-only connections allocate no
/// shared memory and pay no startup `TRANSFER` RTT. Cancellation-safe and safe under
/// [`spawn_on_replies`] (which polls inline inside `Waker::wake`): locks are always dropped before
/// waking waiters, and an in-flight `TRANSFER` stays alive so a later caller awaits the same reply
/// instead of sending a duplicate `TRANSFER` (`EEXIST`).
struct LazyTransfer {
    ready: OnceLock<Arc<Transfer>>,
    init: Mutex<Option<Arc<TransferInFlight>>>,
}

struct TransferInFlight {
    state: Mutex<InFlightState>,
}

struct InFlightState {
    reply: Option<BoxFuture<'static, Result<Arc<Transfer>>>>,
    done: Option<Result<Arc<Transfer>>>,
    wakes: u64,
    waiters: Vec<std::task::Waker>,
}

impl std::task::Wake for TransferInFlight {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let waiters = {
            let mut state = self.state.lock().unwrap();
            state.wakes = state.wakes.wrapping_add(1);
            std::mem::take(&mut state.waiters)
        };
        for waker in waiters {
            waker.wake();
        }
    }
}

impl LazyTransfer {
    fn new() -> Self {
        Self { ready: OnceLock::new(), init: Mutex::new(None) }
    }

    #[cfg(any(test, feature = "testing"))]
    fn get(&self) -> Option<&Arc<Transfer>> {
        self.ready.get()
    }

    async fn get_or_init(&self, client: &Arc<Client>) -> Result<Arc<Transfer>> {
        if let Some(transfer) = self.ready.get() {
            return Ok(transfer.clone());
        }
        let in_flight = {
            let mut init = self.init.lock().unwrap();
            if let Some(transfer) = self.ready.get() {
                return Ok(transfer.clone());
            }
            if let Some(existing) = init.as_ref() {
                existing.clone()
            } else {
                let client = client.clone();
                let reply: BoxFuture<'static, Result<Arc<Transfer>>> = Box::pin(async move {
                    let (transfer, file) = Transfer::make()
                        .map_err(|err| err.raw_os_error().map_or(Errno::EIO, Errno::from_raw))?;
                    let reply = {
                        let area = Request::Transfer {
                            len: transfer.len() as u64,
                            area: Area(file.as_fd()),
                        };
                        client.request(&area)
                    };
                    drop(file);
                    reply.await?;
                    Ok(Arc::new(transfer))
                });
                let created = Arc::new(TransferInFlight {
                    state: Mutex::new(InFlightState {
                        reply: Some(reply),
                        done: None,
                        wakes: 0,
                        waiters: Vec::new(),
                    }),
                });
                *init = Some(created.clone());
                created
            }
        };
        std::future::poll_fn(|cx| {
            loop {
                let (mut fut, epoch) = {
                    let mut state = in_flight.state.lock().unwrap();
                    if let Some(result) = &state.done {
                        return std::task::Poll::Ready(result.clone());
                    }
                    match state.reply.take() {
                        Some(fut) => (fut, state.wakes),
                        None => {
                            state.waiters.push(cx.waker().clone());
                            return std::task::Poll::Pending;
                        }
                    }
                };
                let waker = std::task::Waker::from(in_flight.clone());
                let mut sub_cx = std::task::Context::from_waker(&waker);
                match fut.as_mut().poll(&mut sub_cx) {
                    std::task::Poll::Ready(result) => {
                        if let Ok(transfer) = &result {
                            self.ready.get_or_init(|| transfer.clone());
                        }
                        let waiters = {
                            let mut state = in_flight.state.lock().unwrap();
                            state.done = Some(result.clone());
                            std::mem::take(&mut state.waiters)
                        };
                        {
                            let mut init = self.init.lock().unwrap();
                            if init.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &in_flight)) {
                                *init = None;
                            }
                        }
                        for waiter in waiters {
                            waiter.wake();
                        }
                        return std::task::Poll::Ready(result);
                    }
                    std::task::Poll::Pending => {
                        let mut state = in_flight.state.lock().unwrap();
                        state.reply = Some(fut);
                        if state.wakes != epoch {
                            continue;
                        }
                        state.waiters.push(cx.waker().clone());
                        return std::task::Poll::Pending;
                    }
                }
            }
        })
        .await
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

    /// The bytes of the area.
    fn len(&self) -> usize {
        Self::SLOT * Self::SLOTS
    }

    /// A transfer area, mapped, pre-populated once, and fixed at its size ([`fixed_shared_memory`]).
    fn make() -> std::io::Result<(Self, std::fs::File)> {
        let len = Self::SLOT * Self::SLOTS;
        let file = fixed_shared_memory(len as u64)?;
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
        // cladefs: write every page once before handing the memfd to the daemon, so the kernel
        // charges every page to this VM process and the daemon can verify hole-free once at
        // `TransferArea::map` (TASK-547, board 11707, hotbed question 575). Written, not advised
        // (`MADV_POPULATE_WRITE`): a VM process's seccomp filter allows no more calls than it
        // needs.
        let page_size = cladefs_core::pages::page_size() as usize;
        for page in (0..len).step_by(page_size) {
            // SAFETY: Within the mapping we just made, before sharing it with the daemon.
            unsafe { addr.cast::<u8>().add(page).write_volatile(0) };
        }
        let transfer = Self {
            addr: NonNull::new(addr.cast()).unwrap(),
            free: Mutex::new((0..Self::SLOTS).rev().collect()),
            used: AtomicU64::new(0),
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
        Some(Slot(Arc::new(TakenSlot { transfer: transfer.clone(), index })))
    }
}

/// cladefs: shared memory of `len` bytes, which no process can shrink, as a transfer area's must
/// not, as the daemon maps it only so and would fault past a new end. On Linux, a memfd sealed at
/// its size: not against more seals, as the daemon seals it against writes once it mapped it
/// (TASK-547). On macOS, which has no memfd nor seals, a POSIX shared memory object, unlinked as
/// it is made: the kernel resizes one never again once it is sized, which the daemon checks
/// (`server::seals`, board 13065, hotbed question 566).
#[cfg(target_os = "linux")]
fn fixed_shared_memory(len: u64) -> std::io::Result<std::fs::File> {
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
    file.set_len(len)?;
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW;
    // SAFETY: Seals the descriptor's memory, which this process made.
    if unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&file), libc::F_ADD_SEALS, seals) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

/// cladefs: [`fixed_shared_memory`] on macOS: a POSIX shared memory object, of a random name, which
/// no other process of the user's guesses to open before it is unlinked, at once (made anew if one
/// was there by it), then sized once and for all.
#[cfg(not(target_os = "linux"))]
fn fixed_shared_memory(len: u64) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let (name, fd) = loop {
        // Within macOS's 31 bytes of a name (PSHMNAMLEN).
        let name = format!("/cladefs-{:016x}", rand::random::<u64>());
        let name = std::ffi::CString::new(name).map_err(std::io::Error::other)?;
        // SAFETY: The name is a C string; the mode an int, as the variadic call takes it. Not
        // `O_CLOEXEC`, which macOS's `shm_open` refuses (`EINVAL`): set below.
        let fd = unsafe {
            libc::shm_open(
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::c_uint,
            )
        };
        if fd >= 0 {
            break (name, fd);
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(err);
        }
    };
    // SAFETY: Just created, and owned here.
    let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    // SAFETY: Sets a flag of the descriptor this process just made.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: Unlinks the name just made, whose object `file` keeps.
    if unsafe { libc::shm_unlink(name.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    file.set_len(len)?;
    Ok(file)
}

impl Drop for Transfer {
    fn drop(&mut self) {
        // SAFETY: Unmaps the area, which no slot is had of any more.
        unsafe { libc::munmap(self.addr.as_ptr().cast(), self.len()) };
    }
}

/// The bytes of a transfer area's slot, for tests.
#[cfg(any(test, feature = "testing"))]
pub const TRANSFER_SLOT: usize = Transfer::SLOT;

/// The slots of a transfer area, for tests.
#[cfg(any(test, feature = "testing"))]
pub const TRANSFER_SLOTS: usize = Transfer::SLOTS;

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

    fn fault_page(&self, offset: u64) -> BoxFuture<'_, Result<()>> {
        self.fault_with(Fault { offset, probe: false })
    }

    fn probe(&self, offset: u64) -> BoxFuture<'_, Result<()>> {
        self.fault_with(Fault { offset, probe: true })
    }

    fn lost(&self) -> bool {
        self.client.lost.happened()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::Opened;
    use cladefs_core::pages::ArenaId;

    /// board 11697 (hotbed review 165): a non-ask `CAPACITY` grow notification queued after the
    /// daemon published the writer and received before `OPEN`'s reply is decoded must not be
    /// overwritten by the older capacity snapshot carried in `OPEN`'s reply.
    #[test]
    fn open_does_not_overwrite_concurrent_capacity_grow() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let sender = Sender::new(server_stream.try_clone().unwrap()).unwrap();
            let mut receiver = Receiver::new(server_stream);
            let open = receiver.receive(|| 0).unwrap().unwrap();
            assert_eq!(open.header.kind, OPEN);
            // Concurrent grow arrives before OPEN's reply.
            let (grow_header, grow_payload) =
                Told::Capacity { capacity: 4 << 20, ask: false }.encode();
            sender.send(grow_header, &grow_payload, &[]).unwrap();
            // Now send OPEN's reply carrying the older 2 MiB capacity snapshot.
            let opened = Opened {
                cell: 7,
                tenant: 1,
                arena: ArenaId::DEFAULT,
                max_map_count: None,
                ephemeral: false,
                capacity: 2 << 20,
            };
            let reply = Header::new(REPLY, open.header.id).with_body(&opened.to_body());
            let own = tempfile::tempfile().unwrap();
            let public = tempfile::tempfile().unwrap();
            sender.send(reply, &opened.capacity_bytes(), &[own.as_fd(), public.as_fd()]).unwrap();
        });

        let client =
            CellClient::over(client_stream, Path::new("store"), Opening::Existing).unwrap();
        assert_eq!(
            client.capacity(),
            4 << 20,
            "OPEN reply must not clobber a newer concurrent CAPACITY grow"
        );
        drop(client);
        server.join().unwrap();
    }
}
