// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device's VMM-independent state: its cell, its configuration space and features, its
//! notices, its DAX window, and its snapshots. A VMM's adapter holds a [`Device`] beside its
//! transport's state (queues, interrupts, the window's BAR), and calls it as the driver
//! activates and resets the device, and as the VM is snapshotted and restored.

use std::sync::Arc;

use cladefs::{CellClient, DaemonConnection, WindowMapping};
use log::{error, info};
use zerocopy::FromZeros;

use crate::config::{Config, private_max};
use crate::notices::{ConfigInterrupt, Notices};
use crate::persist::{ConfigState, MappingState, Snapshot, SnapshotError, SnapshotMode, State};
use crate::protocol::{
    Bounds, ConfigSpace, DAX_WINDOW_SHM_ID, MAX_EXTENT, MAX_ROOT, MAX_TAG_LEN, MAX_TRANSFER,
    QUEUE_SIZE, VIRTIO_EXTENT_F_STATS, VIRTIO_EXTENT_VERSION,
};
use crate::requests::Requests;
use crate::window::{DaxWindowError, PrefaultGuestMemory, WindowMemory, WindowVm};
use crate::workers::{RequestQueue, WorkerQueue, Workers};

/// What the VMM does as a device's extent daemon goes: stops the VM, the guest's disk gone,
/// rather than going on with every request failing (TASK-50.2). Firecracker exits, saying why.
pub type OnLost = Arc<dyn Fn() + Send + Sync>;

/// Errors making or reopening a device.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Cannot open the cell {0}: {1:#}")]
    Cell(String, cladefs::Error),
    #[error(
        "Extent device {id}: opened cell {found:x}, expected cell {expected:x} (the path does not \
         hold the migrated cell)"
    )]
    CellMismatch { id: String, expected: u64, found: u64 },
    #[error("The tag {0:?} is longer than 36 bytes")]
    TagTooLong(String),
    #[error("Cannot make the DAX window: {0}")]
    DaxWindow(#[from] DaxWindowError),
    #[error(
        "The snapshot's extent device {0} has a DAX window configured but no address saved for it"
    )]
    WindowNotSaved(String),
    #[error("The extent device {0} has a DAX window, which cannot take its guest's faults: {1:#}")]
    Userfaultfd(String, cladefs::Error),
    #[error("Extent device {0}: recreating its DAX window after reopen: {1:#}")]
    ReopenWindow(String, cladefs::Error),
    #[error("Extent device {0}: remapping its DAX window after reopen: {1}")]
    ReopenMappings(String, cladefs::Errno),
}

/// Errors releasing a paused device's cell ([`Device::release_cell`]).
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error(
        "Extent device {0}: its DAX window holds {1} private pages, whose bytes no extent holds \
         (sync or freeze the guest's filesystem before releasing its cell)"
    )]
    Private(String, u64),
    #[error(
        "Extent device {0}: its cell has uncommitted open extents (sync or freeze the guest's \
         filesystem before releasing its cell)"
    )]
    NotQuiescent(String),
    #[error("Extent device {0}: cannot list its DAX window's mappings before release: {1}")]
    Mappings(String, cladefs::Errno),
}

/// Errors activating a device.
#[derive(Debug, thiserror::Error)]
pub enum ActivateError {
    #[error("creating the DAX window: {0:#}")]
    Window(cladefs::Error),
    #[error("mapping the snapshot's window again: {0}")]
    MapAgain(cladefs::Errno),
}

/// The device's DAX window as its transport describes it: shared memory region `id`, of `len`
/// bytes at `guest_addr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShmRegion {
    pub id: u8,
    pub guest_addr: u64,
    pub len: u64,
}

/// How a device is made: as its VM boots, or restored from a snapshot, its window where the
/// snapshot had it in guest memory.
enum Making<'a> {
    Boot,
    Restore { snapshot: &'a Snapshot, window_addr: Option<u64> },
}

/// A virtio extent-store device's VMM-independent half, on an extent store that cladefs's extent
/// daemon serves.
pub struct Device {
    config: Config,
    // The cell before the window, whose host memory the cell's fault thread serves: the cell
    // is dropped first. `None` while the cell is released on a paused VM for live-migration
    // hand-over (`Device::release_cell`).
    cell: Option<Arc<CellClient>>,
    /// The opened cell's `(tenant, cell)` ID, kept so `reopen_cell` can reopen managed JBOD cells
    /// that were initially opened with a `<tenant>/0` create/fork sentinel.
    cell_id: cladefs::CellId,
    /// Callback to stop the VM if the extent daemon goes; kept for `reopen_cell`.
    on_lost: OnLost,
    window: Option<WindowMemory>,
    /// Whether the cell's window is created, which it is on the first activation.
    window_created: bool,
    /// Maps the window's pages for the guest as the cell maps them.
    prefault: Option<Arc<dyn PrefaultGuestMemory>>,
    config_space: ConfigSpace,
    /// The cell's capacity, as the daemon changes it; cladefs: and whether the guest's filesystem
    /// is frozen (`VIRTIO_EXTENT_T_FREEZE`), as a snapshot needs it.
    notices: Arc<Notices>,
    /// cladefs: the snapshot taken of the device's cell, or restored from (TASK-39.1).
    snapshot: Option<Snapshot>,
    /// cladefs: the window's mappings to map again as the window is created: those of the snapshot
    /// the device was restored from.
    mappings: Vec<WindowMapping>,
    /// Window mappings captured when the cell was released (`Device::release_cell`), restored if
    /// `Device::reopen_cell` reopens the cell after a migration abort.
    released_mappings: Option<Vec<WindowMapping>>,
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("config", &self.config)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl Device {
    /// A device as `config` describes, with `queues` request queues, as its VM boots: opens its
    /// cell, creating it if it doesn't exist, in the extent daemon listening at its
    /// `cladefsd_socket`, and maps its DAX window, if it has one, into `vm`'s guest. `on_lost` stops
    /// the VM as the daemon goes.
    pub fn new(
        config: Config,
        queues: u16,
        vm: Arc<dyn WindowVm>,
        on_lost: OnLost,
    ) -> Result<Self, Error> {
        Self::make(config, Daemon::Socket, queues, Making::Boot, vm, on_lost)
    }

    /// [`Device::new`], its cell opened over `daemon`, a connection the VMM made, rather than at
    /// `config`'s socket, which it ignores: the device's one seam to its daemon. libmachine's
    /// connects in memory to the daemon of its own process (libcladefs, board 13471), or, from a
    /// helper process of a server's, over the wire to the server's (board 13460).
    pub fn connected(
        config: Config,
        daemon: DaemonConnection,
        queues: u16,
        vm: Arc<dyn WindowVm>,
        on_lost: OnLost,
    ) -> Result<Self, Error> {
        Self::make(config, Daemon::Connected(daemon), queues, Making::Boot, vm, on_lost)
    }

    /// The device a snapshot's `state` holds, with `queues` request queues, as many as it had: its
    /// cell opened, or forked from the snapshot's version if it doesn't exist; its window in guest
    /// memory where the snapshot had it; the guest's filesystem frozen as it was, and its window's
    /// mappings mapped again as the driver activates it, as it was. The VMM checks the
    /// snapshot's protocol version first ([`check_version`](crate::protocol::check_version)), and
    /// restores its transport's state.
    pub fn restore(
        state: &State,
        queues: u16,
        vm: Arc<dyn WindowVm>,
        on_lost: OnLost,
    ) -> Result<Self, Error> {
        let snapshot = Snapshot::from(state);
        let making = Making::Restore { snapshot: &snapshot, window_addr: state.window_addr };
        let config = Config::from(&state.config);
        let mut device = Self::make(config, Daemon::Socket, queues, making, vm, on_lost)?;
        device.notices.set_frozen(state.frozen);
        device.mappings.clone_from(&snapshot.mappings);
        device.snapshot = Some(snapshot);
        Ok(device)
    }

    /// A device as `config` describes, with `queues` request queues, made as `making` says, on
    /// `daemon`: its cell opened, or, restored from a snapshot, forked from the snapshot's version
    /// if it doesn't exist; its window in guest memory where the snapshot had it.
    fn make(
        config: Config,
        daemon: Daemon,
        queues: u16,
        making: Making<'_>,
        vm: Arc<dyn WindowVm>,
        on_lost: OnLost,
    ) -> Result<Self, Error> {
        let mut config_space = ConfigSpace::new_zeroed();
        config_space.version = VIRTIO_EXTENT_VERSION.to_le();
        config_space.num_request_queues = u32::from(queues).to_le();
        config_space.block_size = cladefs::OpenCell::BLOCK_SIZE.to_le();
        // cladefs: MAP, UNMAP and PRIVATE accept cell-block-aligned (4 KiB) ranges even on hosts
        // with larger pages (such as 16 KiB arm64), where the daemon composes unaligned host pages
        // and the second stage tracks sub-page private state, so 4 KiB guests enable DAX cleanly.
        config_space.map_alignment = cladefs::OpenCell::BLOCK_SIZE.to_le();
        config_space.max_transfer = MAX_TRANSFER.to_le();
        config_space.max_segments = (u32::from(QUEUE_SIZE) - 2).to_le();
        config_space.max_root = MAX_ROOT.to_le();
        config_space.max_extent = MAX_EXTENT.to_le();
        let tag = config.tag.as_bytes();
        if tag.len() > MAX_TAG_LEN {
            return Err(Error::TagTooLong(config.tag.clone()));
        }
        config_space.tag[..tag.len()].copy_from_slice(tag);

        // cladefs: a window's faults come through /dev/userfaultfd, which the device opens as the
        // driver activates it: a host that denies it fails the VM now, saying what it lacks, not
        // the guest's activation, after which the VM has no disk (TASK-308).
        #[cfg(target_os = "linux")]
        if config.dax_window_size.is_some() {
            cladefs::userfaultfd_accessible()
                .map_err(|err| Error::Userfaultfd(config.id.clone(), err))?;
        }
        let daemon = match daemon {
            Daemon::Socket => DaemonConnection::connect(&config.cladefsd_socket),
            Daemon::Connected(daemon) => Ok(daemon),
        };
        let cell = daemon
            .map(|daemon| hear_lost(daemon, &config.id, &on_lost))
            .and_then(|daemon| match &making {
                Making::Boot => daemon.open(&config.path_on_host, config.capacity),
                Making::Restore { snapshot, .. } => match snapshot.target {
                    cladefs::CellSnapshot::Version(version) if version.version == 0 => {
                        let target = if config
                            .path_on_host
                            .parse::<cladefs::CellId>()
                            .is_ok_and(|c| c.id == 0)
                            && version.cell != 0
                        {
                            cladefs::CellId { tenant: version.tenant, id: version.cell }.to_string()
                        } else {
                            config.path_on_host.clone()
                        };
                        daemon.open_existing(&target)
                    }
                    cladefs::CellSnapshot::Version(version) => {
                        daemon.open_forked(&config.path_on_host, version, config.capacity)
                    }
                    cladefs::CellSnapshot::LocalEphemeral { tenant, cell, committed } => daemon
                        .open_ephemeral_at_commit(
                            &config.path_on_host,
                            cladefs::CellId { tenant, id: cell },
                            committed,
                            config.capacity,
                        ),
                },
            })
            .map_err(|err| Error::Cell(config.path_on_host.clone(), err))?;
        if let Making::Restore { snapshot, .. } = &making
            && let cladefs::CellSnapshot::Version(version) = snapshot.target
            && version.version == 0
            && version.cell != 0
            && cell.cell() != version.cell
        {
            return Err(Error::CellMismatch {
                id: config.id,
                expected: version.cell,
                found: cell.cell(),
            });
        }
        config_space.cell = cell.cell().to_le();
        let notices = Arc::new(Notices::default());
        notices.resizing.opened(cell.capacity());
        let changes = notices.clone();
        cell.on_capacity(move |capacity, ask| changes.resizing.changed(capacity, ask));
        hear_stats_asks(&cell, &notices);

        let window = match config.dax_window_size {
            Some(size) => {
                let at = match making {
                    Making::Boot => None,
                    Making::Restore { window_addr: Some(addr), .. } => Some(addr),
                    Making::Restore { window_addr: None, .. } => {
                        return Err(Error::WindowNotSaved(config.id.clone()));
                    }
                };
                let window = WindowMemory::new(vm, size, at)?;
                // cladefs: the window copies on write (backlog doc-13).
                config_space.private_max =
                    private_max(window.len(), config.dax_private_max_bytes).to_le();
                Some(window)
            }
            None => None,
        };

        let cell_id = cladefs::CellId { tenant: cell.tenant(), id: cell.cell() };
        Ok(Self {
            config,
            cell: Some(Arc::new(cell)),
            cell_id,
            on_lost,
            window,
            window_created: false,
            prefault: None,
            config_space,
            notices,
            snapshot: None,
            mappings: Vec::new(),
            released_mappings: None,
        })
    }

    /// Its configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Has `prefault` map the DAX window's pages for the guest as the cell maps them, rather
    /// than the guest faulting on each.
    pub fn set_prefault(&mut self, prefault: Arc<dyn PrefaultGuestMemory>) {
        self.prefault = Some(prefault);
    }

    /// The device's own features ([`Config::features`]): the VMM offers its transport's beside
    /// them, and refuses a driver that takes fewer than every one
    /// ([`check_acked`](crate::protocol::check_acked)).
    pub fn features(&self) -> u64 {
        self.config.features()
    }

    /// The DAX window, as the transport describes it to the driver, if the device has one.
    pub fn shm_region(&self) -> Option<ShmRegion> {
        let window = self.window.as_ref()?;
        Some(ShmRegion {
            id: DAX_WINDOW_SHM_ID,
            guest_addr: window.guest_addr(),
            len: window.len(),
        })
    }

    /// Reads `data.len()` bytes of the configuration space at `offset`, as the driver does, past
    /// its end zeros. cladefs: the cell's capacity, and the shrink the guest is asked to take, as
    /// they are now; and the daemon's latest ask for the guest's stats.
    pub fn read_config(&self, offset: u64, data: &mut [u8]) {
        let (capacity, asked) = self.notices.resizing.known();
        let config = ConfigSpace {
            capacity: capacity.to_le(),
            asked: asked.to_le(),
            stats_asked: self.notices.stats.latest().to_le(),
            ..self.config_space
        };
        config.read(offset, data);
    }

    /// Its cell (panics if called while released; callers check [`Self::is_cell_released`]).
    fn cell(&self) -> &Arc<CellClient> {
        self.cell.as_ref().expect("extent device cell is open")
    }

    /// Whether the device's cell has been released on a paused VM ([`Self::release_cell`]).
    pub fn is_cell_released(&self) -> bool {
        self.cell.is_none()
    }

    /// The requests of one of its queues, served with its cell, while the device is activated:
    /// for a worker taking them off the queue, as [`Device::start_workers`]'s do.
    pub fn requests(&self) -> Requests {
        let window = self.window.as_ref().map_or(0, WindowMemory::len);
        let bounds = Bounds::of(&self.config_space, window);
        Requests::new(self.cell().clone(), self.notices.clone(), self.config.cache_type, bounds)
    }

    /// Starts the workers serving `queues`, those the driver set up, the device activated
    /// ([`Device::activate`]): a thread each, named `name` and its queue's index, all of them or
    /// none. As the VM is paused for a snapshot, or the device reset, the VMM stops them
    /// ([`Workers::stop`]), and takes their queues back; it starts them again as the VM resumes.
    pub fn start_workers<Q: RequestQueue>(
        &self,
        queues: Vec<WorkerQueue<Q>>,
        name: &str,
    ) -> std::io::Result<Workers<Q>> {
        Workers::start(queues, name, || Arc::new(self.requests()))
    }

    /// Activates the device, the driver having taken features `acked`, every one offered, and its
    /// transport set up the queues: makes the DAX window, maps again the mappings of the snapshot it was restored from, and has
    /// `interrupt` tell the guest of the daemon's notices. The VMM's workers then serve the queues
    /// ([`Device::requests`]). On a thread under the VMM's seccomp filter, rather than the vCPU's
    /// whose write activates the device: its work, opening the DAX window's userfaultfd and the
    /// like, is the VMM's, not a vCPU's (TASK-50.1).
    pub fn activate(
        &mut self,
        acked: u64,
        interrupt: Arc<dyn ConfigInterrupt>,
    ) -> Result<(), ActivateError> {
        self.create_dax_window().map_err(ActivateError::Window)?;
        self.map_again().map_err(ActivateError::MapAgain)?;
        self.notices.resizing.set_interrupt(Some(interrupt.clone()));
        // cladefs: the guest is asked for its stats if its driver takes asks (TASK-111.4).
        let asks = acked & (1 << VIRTIO_EXTENT_F_STATS) != 0;
        self.notices.stats.set_interrupt(asks.then_some(interrupt));
        Ok(())
    }

    /// Resets the device, its VMM's workers stopped: as before the driver activated it. On a
    /// thread under the VMM's seccomp filter, as activation.
    pub fn reset(&mut self) {
        // cladefs: release the transport's interrupt references so a PCI reset can tear down its
        // MSI-X vector group and no notice fires while inactive; no guest is asked for its stats
        // until a driver activates it again, the cell answering meanwhile (TASK-111.4).
        self.notices.resizing.set_interrupt(None);
        self.notices.stats.set_interrupt(None);
    }

    /// Creates the cell's DAX window in the window's host memory, unless the device has no
    /// window, or it exists.
    fn create_dax_window(&mut self) -> Result<(), cladefs::Error> {
        let Some(window) = &self.window else {
            return Ok(());
        };
        if self.window_created {
            return Ok(());
        }
        // cladefs: it copies on write, of `private_max` pages at most (backlog doc-13).
        let private_max = u64::from_le(self.config_space.private_max);
        // SAFETY: The cell is dropped before the window, as the fields are in that order.
        #[cfg(target_os = "linux")]
        unsafe { window.create(self.cell(), self.prefault.clone(), private_max) }?;
        // The VM maps the window's pages itself, as the cell's stage says (board 13069).
        #[cfg(not(target_os = "linux"))]
        window.create(self.cell(), private_max)?;
        self.window_created = true;
        Ok(())
    }

    /// Maps the window's mappings of the snapshot the device was restored from again. cladefs: all
    /// at once, in a request of the daemon's or a few, each one change of the window, where a MAP
    /// each, a round trip apiece, held the restore up (TASK-92.14).
    fn map_again(&mut self) -> Result<(), cladefs::Errno> {
        let mappings = std::mem::take(&mut self.mappings);
        if mappings.is_empty() {
            return Ok(());
        }
        let start = std::time::Instant::now();
        self.cell().map_many(&mappings)?;
        info!(
            "extent device {}: the snapshot's {} window mappings mapped again in {:?}",
            self.config.id,
            mappings.len(),
            start.elapsed()
        );
        Ok(())
    }

    /// cladefs: takes a versioned snapshot of the device's cell ([`SnapshotMode::Versioned`]).
    pub fn take_snapshot(&mut self, quiesce: impl FnOnce()) -> Result<(), SnapshotError> {
        self.take_snapshot_with(SnapshotMode::Versioned, quiesce)
    }

    /// cladefs: takes a snapshot of the device's cell according to `mode`:
    /// - [`SnapshotMode::Versioned`]: requires the guest's filesystem to be frozen, stops workers
    ///   via `quiesce`, verifies no private DAX pages exist, makes a cell version, and records the
    ///   window's mappings.
    /// - [`SnapshotMode::Migration`]: skips the guest frozen check and makes no cell version
    ///   (`version.version == 0`, recording `version.cell` for restore verification), stops workers
    ///   via `quiesce`, verifies no private DAX pages and no uncommitted open extents exist, and
    ///   records the window's mappings for a cell moved whole via `HandOverCell`.
    pub fn take_snapshot_with(
        &mut self,
        mode: SnapshotMode,
        quiesce: impl FnOnce(),
    ) -> Result<(), SnapshotError> {
        let id = self.config.id.clone();
        if self.cell.is_none() {
            return Err(SnapshotError::Released(id));
        }
        if mode == SnapshotMode::Versioned && !self.notices.frozen() {
            return Err(SnapshotError::NotFrozen(id));
        }
        quiesce();
        let private = self.cell().private_pages();
        if private > 0 {
            return Err(SnapshotError::Private(id, private));
        }
        match mode {
            SnapshotMode::Versioned => {
                let target = self
                    .cell()
                    .snapshot_cell()
                    .map_err(|errno| SnapshotError::Version(id.clone(), errno))?;
                let mappings = match self.cell().window_mappings() {
                    Ok(mappings) => mappings,
                    Err(errno) => {
                        if let cladefs::CellSnapshot::Version(version) = target {
                            self.drop_version(version.version);
                        }
                        return Err(SnapshotError::Mappings(id, errno));
                    }
                };
                self.snapshot = Some(Snapshot { target, mappings });
            }
            SnapshotMode::Migration => {
                let mappings = self.cell().window_mappings_quiescent().map_err(|errno| {
                    if errno == cladefs::Errno::EBUSY {
                        SnapshotError::NotQuiescent(id.clone())
                    } else {
                        SnapshotError::Mappings(id.clone(), errno)
                    }
                })?;
                let target = cladefs::CellSnapshot::Version(cladefs::CellVersion {
                    tenant: self.cell().tenant(),
                    cell: self.cell().cell(),
                    version: 0,
                });
                self.snapshot = Some(Snapshot { target, mappings });
            }
        }
        Ok(())
    }

    /// cladefs: closes the device's cell connection on a paused VM so `HandOverCell` can acquire
    /// the cell alone, keeping the VM process and its DAX window memory reservation intact so
    /// [`Device::reopen_cell`] can take the cell back if migration aborts. Idempotent if already
    /// released. Refuses if private DAX pages or uncommitted open extents exist.
    pub fn release_cell(&mut self, quiesce: impl FnOnce()) -> Result<(), ReleaseError> {
        if self.cell.is_none() {
            return Ok(());
        }
        quiesce();
        let id = self.config.id.clone();
        let private = self.cell().private_pages();
        if private > 0 {
            return Err(ReleaseError::Private(id, private));
        }
        let mappings = self.cell().window_mappings_quiescent().map_err(|errno| {
            if errno == cladefs::Errno::EBUSY {
                ReleaseError::NotQuiescent(id.clone())
            } else {
                ReleaseError::Mappings(id.clone(), errno)
            }
        })?;
        if self.window_created {
            self.released_mappings = Some(mappings);
            self.window_created = false;
        }
        // Drops the last `Arc<CellClient>` (workers were stopped by `quiesce`), synchronously
        // releasing the uffd stage, joining the fault thread, and shutting down the daemon socket
        // without firing `on_lost`.
        drop(self.cell.take());
        Ok(())
    }

    /// cladefs: reopens the device's cell on a paused VM after [`Device::release_cell`] (e.g., on
    /// migration abort), recreating its DAX window and restoring the window mappings captured on
    /// release. Idempotent if already open. On error, keeps the cell released and preserves the
    /// captured mappings so the caller may retry. It reconnects at the configuration's socket: a
    /// device made over a connection of the VMM's ([`Device::connected`]) has none to reopen over
    /// yet, libmachine's migrating none.
    pub fn reopen_cell(&mut self) -> Result<(), Error> {
        if self.cell.is_some() {
            return Ok(());
        }
        let target = if self.config.path_on_host.parse::<cladefs::CellId>().is_ok_and(|c| c.id == 0)
        {
            self.cell_id.to_string()
        } else {
            self.config.path_on_host.clone()
        };
        let cell = DaemonConnection::connect(&self.config.cladefsd_socket)
            .map(|daemon| hear_lost(daemon, &self.config.id, &self.on_lost))
            .and_then(|daemon| daemon.open_existing(&target))
            .map_err(|err| Error::Cell(target.clone(), err))?;
        let expected = u64::from_le(self.config_space.cell);
        if cell.cell() != expected {
            return Err(Error::CellMismatch {
                id: self.config.id.clone(),
                expected,
                found: cell.cell(),
            });
        }
        self.notices.resizing.opened(cell.capacity());
        let changes = self.notices.clone();
        cell.on_capacity(move |capacity, ask| changes.resizing.changed(capacity, ask));
        hear_stats_asks(&cell, &self.notices);
        self.cell = Some(Arc::new(cell));
        if let Some(mappings) = self.released_mappings.take() {
            let result = self
                .create_dax_window()
                .map_err(|err| Error::ReopenWindow(self.config.id.clone(), err))
                .and_then(|()| {
                    if mappings.is_empty() {
                        return Ok(());
                    }
                    self.cell()
                        .map_many(&mappings)
                        .map_err(|errno| Error::ReopenMappings(self.config.id.clone(), errno))
                });
            if let Err(err) = result {
                drop(self.cell.take());
                self.window_created = false;
                self.released_mappings = Some(mappings);
                return Err(err);
            }
        }
        Ok(())
    }

    /// cladefs: drops the snapshot [`Device::take_snapshot`] took, as the VMM does when the VM's
    /// snapshot fails after it: no snapshot file names its version then, which its cell would keep,
    /// and all it holds, for good (TASK-575). The device has no snapshot then. Only a snapshot this
    /// device took: one restored from is the VM manager's (the daemon refuses it, `EPERM`).
    /// Host-local ephemeral snapshots make no bucket version, so dropping them is local-only.
    pub fn drop_snapshot(&mut self) {
        let Some(snapshot) = self.snapshot.take() else { return };
        if let cladefs::CellSnapshot::Version(version) = snapshot.target
            && version.version != 0
            && self.cell.is_some()
        {
            self.drop_version(version.version);
        }
    }

    /// cladefs: drops the version `version` of the cell, which this device made, logging a
    /// drop that fails: the VM's snapshot has failed already, and the version is its cell's to
    /// list and drop.
    fn drop_version(&self, version: u64) {
        if let Err(errno) = self.cell().drop_version(version) {
            error!(
                "extent device {}: dropping version {version:x} of a failed snapshot: {errno}",
                self.config.id
            );
        }
    }

    /// The snapshot taken of the device's cell ([`Device::take_snapshot`]), or restored from.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    /// What a snapshot of its VM holds of the device, once [`Device::take_snapshot`] took the
    /// snapshot of its cell, which the VMM has it do first.
    pub fn state(&self) -> State {
        let snapshot = self
            .snapshot()
            .expect("an extent device's cell is snapshotted before the device is saved");
        State {
            config: ConfigState::from(&self.config),
            window_addr: self.window.as_ref().map(WindowMemory::guest_addr),
            target: snapshot.target,
            mappings: snapshot.mappings.iter().map(MappingState::from).collect(),
            frozen: self.notices.frozen(),
        }
    }
}

/// How a device reaches its cell's extent daemon.
enum Daemon {
    /// At its configuration's `cladefsd_socket`.
    Socket,
    /// Over a connection the VMM made ([`Device::connected`]).
    Connected(DaemonConnection),
}

/// cladefs: `daemon`, the device `id`'s connection to its extent daemon, which has `on_lost` stop
/// the VM once the daemon goes: the guest's disk is gone then, and the VM stops at once, rather
/// than going on with every request failing (TASK-50.2). Said before the cell opens over it, so
/// that a daemon going as it opens, as the VM is created, stops it so too, rather than as a cell
/// that failed to open (TASK-232).
fn hear_lost(daemon: DaemonConnection, id: &str, on_lost: &OnLost) -> DaemonConnection {
    let (id, on_lost) = (id.to_owned(), on_lost.clone());
    daemon.on_lost(move || {
        error!("extent device {id}: the extent daemon went");
        on_lost();
    });
    daemon
}

/// cladefs: has the daemon's asks for the stats of `cell`'s VM process and guest ask the guest,
/// through `notices`, while its driver takes asks; otherwise the cell answers at once, with none
/// of the guest's (TASK-111.4).
fn hear_stats_asks(cell: &CellClient, notices: &Arc<Notices>) {
    let notices = notices.clone();
    cell.on_stats_asked(move |ask| notices.stats.ask(ask));
}
