// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs: the extent device's vhost-user back-end (board 14537, step 3 of board 14378's plan
//! v3): the daemon serves a guest's doc-4 requests off its virtqueues itself, with the
//! interpreter its VMM serves them with (`cladefs_core::extent_requests`), in the guest's
//! dialect, the connection's cell their backend, rather than the VMM translating each into a
//! message on the connection's socket.
//!
//! The channel is a socket pair the daemon makes as the VM process asks ([`crate::wire::VHOST_USER`]), on its
//! connection, once its cell is open: bound to that authorized cell and tenant, as a bare
//! vhost-user socket, with no `OPEN`, couldn't be. Its back-end runs on a thread of its own
//! (`vhost_user::Channel`), stopped as the connection ends.
//!
//! A chain is a request as a message is: checked against the protocol's bounds as it is taken,
//! admitted in the connection's turns ([`super::Work`]), served as a future
//! ([`super::serve_future`]), counted until returned, refused by a halted cell, a call for the
//! guest request its header names. Its buffers are copied out of guest memory and back with
//! `pread` and `pwrite` (plan v3), the copies held in its tenant's bytes in flight while it is
//! served. A panic serving it halts the cell and returns it with nothing written.
//!
//! It serves a stock front-end whose guest RAM is a plain memfd, every page private: no DAX
//! window (`F_DAX`), so no `MAP`, `UNMAP` nor `PRIVATE`. The cell's capacity changes and the
//! daemon's asks for the guest's stats (`F_RESIZE`, `F_STATS`) reach the guest as the device's
//! configuration changes, which the back-end says on the back-end channel (`BACKEND_REQ`, board
//! 14603) rather than the connection telling its VM process ([`GuestState`]); its `STATS` and
//! `RESIZED` answers go to the connection's report and shrinking, as the socket's do. A guest's
//! `FREEZE` the back-end keeps, for the device's state (`DEVICE_STATE`, board 14605).

use super::{Connection, Turn, Work, serve_future};
use crate::reads::Kind;
use crate::wire::{COMMIT, FREE, Header, INFO, MAP, READ, ROOT, UNMAP, WRITE};
use bucket_cell::trace;
use cladefs_core::extent_requests::protocol::{
    self, Bounds, ConfigSpace, MAX_EXTENT, MAX_READABLE, MAX_ROOT, MAX_TAG_LEN, MAX_TRANSFER,
    MAX_WRITABLE, QUEUE_SIZE, RequestHeader, Response, VIRTIO_EXTENT_F_FREE_MANY,
    VIRTIO_EXTENT_F_FREEZE, VIRTIO_EXTENT_F_RESIZE, VIRTIO_EXTENT_F_STATS,
    VIRTIO_EXTENT_F_WRITE_COMMIT, VIRTIO_EXTENT_VERSION,
};
use cladefs_core::extent_requests::{Backend, Buffers, Guest, GuestBackend, GuestNotices, handle};
use cladefs_core::{
    Commit, Errno, ExtentId, ExtentInfo, MappingContents, OpenCell, VolatileSlice,
    VolatileSliceMut, WindowMapping, WriteBarriers,
};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use zerocopy::{FromBytes, FromZeros};

/// `VIRTIO_F_VERSION_1` and `VIRTIO_RING_F_EVENT_IDX`, the transport's features a VMM's extent
/// device offers too (libmachine's, Firecracker's).
const VIRTIO_F_VERSION_1: u32 = 32;
const VIRTIO_RING_F_EVENT_IDX: u32 = 29;

/// The bounds a guest's requests keep to: its device's, with no window.
const BOUNDS: Bounds = Bounds {
    block_size: OpenCell::BLOCK_SIZE as u64,
    max_extent: MAX_EXTENT,
    map_alignment: OpenCell::BLOCK_SIZE as u64,
    window: 0,
};

/// A connection's vhost-user channel's back-end, and its device's guest: none yet; running; or
/// stopped as the connection ends, after which none starts.
#[derive(Default)]
pub(super) enum VhostUser {
    #[default]
    Unstarted,
    Running {
        channel: vhost_user::Channel,
        guest: Arc<GuestState>,
    },
    Stopped,
}

/// What a guest and its vhost-user device tell each other beside its cell's requests: kept for
/// the device's state and configuration, and how its uploaded commits are answered.
#[derive(Debug, Default)]
pub(super) struct GuestState {
    /// cladefs TASK-363: its cache `Unsafe` ([`crate::wire::VHOST_USER_UNSAFE`]).
    ordered_only: bool,
    /// Whether its filesystem is frozen, as it said last (`FREEZE`), which a snapshot's device
    /// state will carry (`DEVICE_STATE`, board 14605).
    frozen: AtomicBool,
    /// What its configuration tells it beside the cell's own.
    told: Mutex<Told>,
    /// How the front-end hears its configuration changed, once the back-end runs.
    notifier: OnceLock<vhost_user::Notifier>,
}

/// What a vhost-user device's configuration tells its guest beside the cell's own, as a VMM's
/// device has it (`virtio_extent::Notices`): the cell's capacity, as the guest is told it; the
/// shrink it is asked to take, 0 if none (`F_RESIZE`); and the latest ask for its stats, 0 before
/// the first (`F_STATS`). Changed and read under its lock: the configuration is read rarely.
#[derive(Clone, Copy, Debug, Default)]
struct Told {
    capacity: u64,
    asked: u64,
    stats_asked: u64,
}

impl GuestState {
    fn new(ordered_only: bool, capacity: u64) -> Self {
        let told = Told { capacity, ..Told::default() };
        Self { ordered_only, told: Mutex::new(told), ..Self::default() }
    }

    fn told(&self) -> Told {
        *self.told.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Tells the front-end the configuration changed: whether it has a back-end channel to hear it
    /// on.
    fn config_changed(&self) -> bool {
        self.notifier.get().is_some_and(vhost_user::Notifier::config_changed)
    }

    /// Tells the guest the cell's capacity is `capacity` bytes, or, if `ask`, asks it to shrink to
    /// it, with a configuration change: `ENOTSUP` for an ask the front-end gave no way to tell,
    /// which nothing would answer.
    pub(super) fn tell_capacity(&self, capacity: u64, ask: bool) -> Result<(), Errno> {
        {
            let mut told = self.told.lock().unwrap_or_else(PoisonError::into_inner);
            if ask {
                told.asked = capacity;
            } else {
                told.capacity = capacity;
                told.asked = 0;
            }
        }
        match self.config_changed() || !ask {
            true => Ok(()),
            false => Err(Errno::ENOTSUP),
        }
    }
}

impl GuestNotices for GuestState {
    fn set_frozen(&self, frozen: bool) {
        self.frozen.store(frozen, Ordering::Release);
    }

    /// The shrink it answered no longer asked, the daemon having asked another since, or told the
    /// capacity it kept, changes nothing (TASK-79.41); the cell hears of it either way
    /// (`GuestBackend::resized`).
    fn resize_answered(&self, capacity: u64, took: bool) {
        let mut told = self.told.lock().unwrap_or_else(PoisonError::into_inner);
        if told.asked != capacity {
            return;
        }
        told.asked = 0;
        if took {
            told.capacity = capacity;
        }
    }
}

/// The daemon asks the guest for its stats with a configuration change, while its driver takes
/// asks (`F_STATS`) and the front-end gave a back-end channel to say it on.
impl crate::stats::AsksGuest for GuestState {
    fn ask(&self, ask: u64) -> bool {
        let acked = self.notifier.get().map_or(0, vhost_user::Notifier::acked);
        if acked & (1 << VIRTIO_EXTENT_F_STATS) == 0 {
            return false;
        }
        self.told.lock().unwrap_or_else(PoisonError::into_inner).stats_asked = ask;
        self.config_changed()
    }
}

/// A connection's extent device, which its vhost-user channel serves.
struct ExtentDevice {
    connection: Arc<Connection>,
    guest: Arc<GuestState>,
    queues: u16,
    /// Its configuration, but what its guest is told beside ([`Told`]).
    config: ConfigSpace,
}

/// The device features the back-end offers: doc-4's of a device with no window, and the
/// transport's.
const FEATURES: u64 = (1 << VIRTIO_EXTENT_F_WRITE_COMMIT)
    | (1 << VIRTIO_EXTENT_F_RESIZE)
    | (1 << VIRTIO_EXTENT_F_FREEZE)
    | (1 << VIRTIO_EXTENT_F_FREE_MANY)
    | (1 << VIRTIO_EXTENT_F_STATS)
    | (1 << VIRTIO_F_VERSION_1)
    | (1 << VIRTIO_RING_F_EVENT_IDX);

impl ExtentDevice {
    fn new(connection: Arc<Connection>, queues: u16, ordered_only: bool, tag: &str) -> Self {
        let open_cell = connection.cell();
        let cell = open_cell.map_or(0, |open_cell| open_cell.cell().id);
        let capacity =
            open_cell.and_then(|open_cell| open_cell.info().ok()).map_or(0, |info| info.capacity);
        let mut config = ConfigSpace::new_zeroed();
        config.version = VIRTIO_EXTENT_VERSION.to_le();
        config.num_request_queues = u32::from(queues).to_le();
        config.block_size = OpenCell::BLOCK_SIZE.to_le();
        config.map_alignment = OpenCell::BLOCK_SIZE.to_le();
        config.max_transfer = MAX_TRANSFER.to_le();
        config.max_segments = (u32::from(QUEUE_SIZE) - 2).to_le();
        config.max_root = MAX_ROOT.to_le();
        config.max_extent = MAX_EXTENT.to_le();
        config.cell = cell.to_le();
        // The wire's decoder took 1 to MAX_TAG_LEN bytes.
        config.tag[..tag.len().min(MAX_TAG_LEN)].copy_from_slice(tag.as_bytes());
        let guest = Arc::new(GuestState::new(ordered_only, capacity));
        Self { connection, guest, queues, config }
    }
}

impl vhost_user::Device for ExtentDevice {
    fn features(&self) -> u64 {
        FEATURES
    }

    /// cladefs TASK-141.3: a driver of the device's protocol takes every feature offered.
    fn check_acked(&self, acked: u64) -> Result<(), String> {
        protocol::check_acked(FEATURES, acked).map_err(|err| err.to_string())
    }

    fn queues(&self) -> u16 {
        self.queues
    }

    fn queue_size(&self) -> u16 {
        QUEUE_SIZE
    }

    fn read_config(&self, offset: u32, data: &mut [u8]) {
        let told = self.guest.told();
        let config = ConfigSpace {
            capacity: told.capacity.to_le(),
            asked: told.asked.to_le(),
            stats_asked: told.stats_asked.to_le(),
            ..self.config
        };
        config.read(u64::from(offset), data);
    }

    /// Admits `chain` in the connection's turns, unless the protocol's bounds refuse it: more
    /// bytes to read than any request has, more to write than any returns, or no room for its
    /// response, returned with nothing written (TASK-79.25, board 2768).
    fn serve(&self, chain: vhost_user::Chain) {
        let (readable, writable) = chain.lens();
        if readable > MAX_READABLE as u64
            || writable > MAX_WRITABLE as u64
            || writable < size_of::<Response>() as u64
        {
            log::warn!(
                "cladefsd: a guest's chain of {readable} bytes to read and {writable} to write, \
                 out of the protocol's bounds: returned with nothing written"
            );
            return chain.complete(0);
        }
        let connection = self.connection.clone();
        if let Some(work) = connection.admit(Work::Chain(self.guest.clone(), chain)) {
            connection.serve_in_turn(work);
        }
    }
}

impl Connection {
    /// Serves a [`crate::wire::VHOST_USER`]: the vhost-user channel of the connection's extent device, of
    /// `queues` request queues, its back-end started, once its cell is open; the reply carries
    /// the front-end's end.
    pub(super) fn serve_vhost_user(
        self: &Arc<Self>,
        queues: u16,
        ordered_only: bool,
        tag: &str,
        answer: super::Answer,
    ) {
        let reply = answer.reply_header();
        // cladefs: where a test holds a start back while the connection ends (failpoints).
        fail::fail_point!("cladefsd::vhost-user-start");
        let started = self.serving().and_then(|_| {
            // cladefs: started under the lock its stop takes, so that none starts once the
            // connection stopped it, which no stop would then reach (the Codex review of board
            // 14537): its back-end would hold the connection, and its cell, for good.
            let mut state = self.vhost_user.lock().unwrap_or_else(PoisonError::into_inner);
            match *state {
                VhostUser::Unstarted => {}
                VhostUser::Running { .. } => return Err(Errno::EEXIST),
                VhostUser::Stopped => return Err(Errno::EBADF),
            }
            let device = Arc::new(ExtentDevice::new(self.clone(), queues, ordered_only, tag));
            let guest = device.guest.clone();
            let (channel, theirs) =
                vhost_user::Channel::start(device, "vhost-user").map_err(|err| {
                    log::error!("cladefsd: starting a vhost-user back-end: {err}");
                    match err.raw_os_error() {
                        Some(libc::EINVAL | libc::EOPNOTSUPP) => Errno::ENOTSUP,
                        Some(raw) => Errno::from_raw(raw),
                        None => Errno::EIO,
                    }
                })?;
            // Set once, the back-end just made.
            let _ = guest.notifier.set(channel.notifier());
            // cladefs board 14603: its guest asked for its stats by the back-end, not the VM
            // process, which may know nothing of the device.
            self.report.asks_guest(Some(guest.clone()));
            *state = VhostUser::Running { channel, guest };
            Ok(theirs)
        });
        match started {
            Ok(theirs) => answer.send(Ok(()), reply, Vec::new(), vec![theirs.as_fd()]),
            Err(errno) => answer.send(Err(errno), reply, Vec::new(), Vec::new()),
        }
    }

    /// Stops its vhost-user channel's back-end, if it has one, as the connection ends: off the
    /// runtime, as its stop waits for its thread.
    pub(super) async fn stop_vhost_user(&self) {
        let stopped = {
            let mut state = self.vhost_user.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *state, VhostUser::Stopped)
        };
        // cladefs: where a test learns the connection stopped its channel (failpoints).
        fail::fail_point!("cladefsd::vhost-user-stopped");
        let VhostUser::Running { channel, .. } = stopped else { return };
        self.report.asks_guest(None);
        if let Err(panicked) = crate::run::off_the_runtime(move || channel.stop()).await {
            log::error!("cladefsd: stopping a vhost-user back-end: {panicked}");
        }
    }

    /// The guest of its vhost-user device, while its back-end runs: who the cell's capacity changes
    /// are told to, rather than its VM process (board 14603).
    pub(super) fn vhost_guest(&self) -> Option<Arc<GuestState>> {
        match &*self.vhost_user.lock().unwrap_or_else(PoisonError::into_inner) {
            VhostUser::Running { guest, .. } => Some(guest.clone()),
            VhostUser::Unstarted | VhostUser::Stopped => None,
        }
    }

    /// Serves `chain`, taken off a queue of its vhost-user device and admitted, in its turn: as a
    /// future ([`serve_future`]) that returns it.
    pub(super) fn serve_chain_in_turn(
        self: Arc<Self>,
        guest: Arc<GuestState>,
        chain: vhost_user::Chain,
    ) {
        let turn = Turn::new(self.clone());
        let connection = self.clone();
        serve_future(turn, async move { connection.serve_chain(&guest, chain).await });
    }

    async fn serve_chain(self: Arc<Self>, guest: &GuestState, chain: vhost_user::Chain) {
        let (readable, writable) = chain.lens();
        // Within the protocol's bounds, a few MiB at most (`ExtentDevice::serve`).
        let (readable, writable) = (readable as usize, writable as usize);
        let mut answer = ChainAnswer::new(self.clone(), chain);
        // cladefs: its copies held in its tenant's bytes in flight, as a socket's request's data is
        // (TASK-149.35): returned with nothing written if the connection ends waiting for room.
        let bytes = (readable + writable) as u64;
        if self.take_in_flight(bytes).await.is_err() {
            return;
        }
        let _held = InFlight { connection: self.clone(), bytes };
        let mut read = vec![0u8; readable];
        if let Err(err) = answer.chain().read(&mut read) {
            log::warn!("cladefsd: reading a guest's request: {err}");
            return;
        }
        let header = RequestHeader::read_from_prefix(&read).ok().map(|(header, _)| header);
        let guest_request = header
            .and_then(|header| std::num::NonZeroU64::new(u64::from_le(header.unique)))
            .map(|guest| cladefs_core::request::RequestId {
                guest,
                open_cell: self.named.get().cloned(),
            });
        let kind = header.and_then(|header| kind_of(u32::from_le(header.type_)));
        answer.named(kind, guest_request.as_ref());
        let mut written = vec![0u8; writable];
        let mut buffers = Buffers::new();
        // SAFETY: The future's own copies, which outlive the request's serving, below, and which
        // nothing else accesses meanwhile.
        unsafe {
            buffers.push_readable(read.as_mut_ptr(), readable);
            buffers.push_writable(written.as_mut_ptr(), writable).expect("within MAX_WRITABLE");
        }
        let dialect = Guest { notices: guest, ordered_only: guest.ordered_only };
        let parent = answer.span.parent();
        let served = handle(&*self, &dialect, BOUNDS, buffers);
        let handled = cladefs_core::request::within(guest_request, parent.around(served)).await;
        // What the device wrote: every writable byte, or nothing (`handle`).
        let len = handled.len as usize;
        if let Err(err) = answer.chain().write(&written[..len]) {
            log::warn!("cladefsd: writing a guest's response: {err}");
            return;
        }
        answer.complete(handled.len, handled.status);
    }

    /// Its cell, to serve a request with: `EBADF` before it opened one, its halting's errno once a
    /// panic halted it (`crate::halt`, TASK-153.16), or once its keys are forgotten (TASK-411.7).
    fn serving(&self) -> Result<&Arc<OpenCell>, Errno> {
        let open_cell = self.cell().ok_or(Errno::EBADF)?;
        match open_cell.halted() {
            Some(errno) => Err(errno),
            None => Ok(open_cell),
        }
    }
}

/// A chain's copies' bytes, held in its tenant's bytes in flight until dropped.
struct InFlight {
    connection: Arc<Connection>,
    bytes: u64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.connection.tenant.in_flight().sub(self.bytes);
    }
}

/// The wire's kind of a doc-4 request of `type_`, which the daemon's metrics and spans count it as.
fn kind_of(type_: u32) -> Option<u32> {
    Some(match type_ {
        protocol::VIRTIO_EXTENT_T_READ => READ,
        protocol::VIRTIO_EXTENT_T_WRITE => WRITE,
        protocol::VIRTIO_EXTENT_T_FREE => FREE,
        protocol::VIRTIO_EXTENT_T_MAP => MAP,
        protocol::VIRTIO_EXTENT_T_UNMAP => UNMAP,
        protocol::VIRTIO_EXTENT_T_COMMIT => COMMIT,
        protocol::VIRTIO_EXTENT_T_ROOT => ROOT,
        protocol::VIRTIO_EXTENT_T_INFO => INFO,
        _ => return None,
    })
}

/// A chain's answer, owed until it is returned ([`ChainAnswer::complete`]): its span, ended as it
/// is, and what the daemon's metrics count of it; and, its serving panicking, the cell halted and
/// the chain returned with nothing written (its `Drop`), rather than the guest waiting for good.
struct ChainAnswer {
    connection: Arc<Connection>,
    chain: Option<vhost_user::Chain>,
    kind: Option<u32>,
    started: std::time::Instant,
    span: trace::Span,
}

impl ChainAnswer {
    fn new(connection: Arc<Connection>, chain: vhost_user::Chain) -> Self {
        let started = std::time::Instant::now();
        Self { connection, chain: Some(chain), kind: None, started, span: trace::Span::none() }
    }

    fn chain(&self) -> &vhost_user::Chain {
        self.chain.as_ref().expect("a chain answered once")
    }

    /// Its request is of the wire's `kind`, a call for the guest request `guest`: its span begun.
    fn named(&mut self, kind: Option<u32>, guest: Option<&cladefs_core::request::RequestId>) {
        self.kind = kind;
        if let Some(kind) = kind {
            let cell = self.connection.cell().map(|open_cell| open_cell.cell());
            self.span = crate::otel::request_span(&Header::new(kind, 0), cell, guest);
        }
    }

    /// Returns the chain, `len` bytes of it written, its request having come to `status`.
    fn complete(mut self, len: u32, status: Result<(), Errno>) {
        if let Err(errno) = status {
            self.span.set(crate::otel::ERRNO, errno.raw() as i64);
            self.span.fail(errno);
        }
        if let Some(kind) = self.kind {
            crate::metrics::served(kind, self.started.elapsed(), status.is_err());
        }
        // Ended before the chain is returned, ready to export.
        drop(std::mem::replace(&mut self.span, trace::Span::none()));
        self.chain.take().expect("a chain answered once").complete(len);
    }
}

impl Drop for ChainAnswer {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(open_cell) = self.connection.cell()
        {
            let kind = self.kind;
            open_cell.halt(|| format!("a panic serving a vhost-user request {kind:?}"));
        }
        // Unreturned, the chain returns itself with nothing written, as it drops.
    }
}

/// A connection's cell, as the backend of its guest's requests on its vhost-user device
/// (`extent_requests`): each its cell's call, as the connection's socket's requests of the same
/// are.
impl Backend for Connection {
    fn cell(&self) -> u64 {
        Connection::cell(self).map_or(0, |open_cell| open_cell.cell().id)
    }

    /// A vhost-user device has no window yet, which would copy on write: no `PRIVATE`, nor
    /// `MAP_ZEROS`.
    fn copies_on_write(&self) -> bool {
        false
    }

    async fn read(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &mut [VolatileSliceMut<'_>],
    ) -> Result<(), Errno> {
        let open_cell = self.serving()?;
        let began = std::time::Instant::now();
        // cladefs: in its turn, as a fault's, as what it pages in holds memory (TASK-149.34).
        let _turn = self.page_ins.acquire().await.map_err(|_| Errno::EIO)?;
        open_cell.read(extent, offset, buffers).await?;
        let len = buffers.iter().map(|buffer| buffer.len() as u64).sum();
        self.traced(Kind::Read, began, open_cell, extent, offset, len);
        Ok(())
    }

    async fn write(
        &self,
        extent: ExtentId,
        offset: u64,
        buffers: &[VolatileSlice<'_>],
        barriers: WriteBarriers<'_>,
    ) -> Result<(), Errno> {
        // cladefs: within its disk and its tenant's limits, as its cell admits it (doc-22).
        self.serving()?.write_with_barriers(extent, offset, buffers, barriers).await
    }

    async fn free_many(&self, extents: &[ExtentId]) -> Result<(), Errno> {
        let open_cell = self.serving()?;
        extents.iter().try_for_each(|extent| open_cell.free(*extent))
    }

    async fn map_many(&self, mappings: &[WindowMapping], warm: bool) -> Result<(), Errno> {
        let open_cell = self.serving()?;
        let began = std::time::Instant::now();
        if warm {
            open_cell.map_many_warm(mappings).await?;
        } else if let [mapping] = mappings {
            match mapping.contents {
                MappingContents::Zeros => {
                    open_cell.map_zeros_at(mapping.window_offset, mapping.len)?
                }
                MappingContents::Extent { extent, offset } => {
                    open_cell.map(extent, offset, mapping.len, mapping.window_offset).await?
                }
            }
        } else {
            open_cell.map_many(mappings).await?;
        }
        self.traced_maps(began, open_cell, mappings);
        Ok(())
    }

    async fn unmap(&self, window_offset: u64, len: u64) -> Result<(), Errno> {
        self.serving()?.unmap(window_offset, len)
    }

    async fn commit(&self, root: Option<&[u8]>, commit: Commit) -> Result<(), Errno> {
        let open_cell = self.serving()?;
        match commit {
            Commit::Uploaded => open_cell.commit(root).await,
            Commit::Local => open_cell.commit_local(root).await,
            Commit::Ordered => open_cell.commit_ordered(root).await,
        }
    }

    async fn root(&self) -> Result<Vec<u8>, Errno> {
        self.serving()?.root()
    }

    async fn info(&self) -> Result<ExtentInfo, Errno> {
        self.serving()?.info()
    }
}

/// What a guest's requests ask of the cell besides, its VMM's elsewhere.
impl GuestBackend for Connection {
    // No window, which a prefault maps.
    fn prefault(&self, _: u64, _: u64) {}

    async fn make_private(&self, _: u64, _: u64) -> Result<(), Errno> {
        Err(Errno::EINVAL)
    }

    async fn answer_stats(&self, ask: u64, guest: Option<&str>) -> Result<(), Errno> {
        self.report.answered(ask, guest)
    }

    async fn resized(&self, capacity: u64, took: bool) -> Result<(), Errno> {
        self.shrinking.answered(capacity, took);
        Ok(())
    }
}
