// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device's VMM-independent state: its store, its configuration space and features, its
//! notices, its DAX window, and its snapshots. A VMM's adapter holds a [`Device`] beside its
//! transport's state (queues, interrupts, the window's BAR), and calls it as the driver
//! activates and resets the device, and as the VM is snapshotted and restored.

use std::path::Path;
use std::sync::Arc;

use cladefs::{DaemonConnection, DaemonExtentStore, WindowMapping};
use log::{error, info};
use zerocopy::FromZeros;

use crate::config::{Config, private_max};
use crate::notices::{ConfigInterrupt, Following, Notices};
use crate::persist::{ConfigState, MappingState, Snapshot, SnapshotError, State};
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

/// Errors making a device.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Cannot open the extent store {0}: {1:#}")]
    Store(String, cladefs::Error),
    #[error("The read-only extent device {0} is restored from no snapshot: it takes none")]
    ReadOnlyRestored(String),
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
}

/// Errors activating a device.
#[derive(Debug, thiserror::Error)]
pub enum ActivateError {
    #[error("opening the store anew: {0:#}")]
    Reopen(cladefs::Error),
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
    // The store before the window, whose host memory the store's fault thread serves: the store
    // is dropped first.
    // cladefs: none from a follower's reset until it is activated again.
    store: Option<Arc<DaemonExtentStore>>,
    window: Option<WindowMemory>,
    /// Whether the store's window is created, which it is on the first activation.
    window_created: bool,
    /// Maps the window's pages for the guest as the store maps them.
    prefault: Option<Arc<dyn PrefaultGuestMemory>>,
    config_space: ConfigSpace,
    /// The store's capacity, as the daemon changes it; cladefs: and whether the guest's filesystem is
    /// frozen (`VIRTIO_EXTENT_T_FREEZE`), as a snapshot needs it.
    notices: Arc<Notices>,
    /// cladefs: a follower's writer's commits, as the daemon tells of them.
    following: Arc<Following>,
    /// What the VMM does as the daemon goes, for each connection to it.
    on_lost: OnLost,
    /// cladefs: the snapshot taken of the device's store, or restored from (TASK-39.1).
    snapshot: Option<Snapshot>,
    /// cladefs: the window's mappings to map again as the window is created: those of the snapshot
    /// the device was restored from.
    mappings: Vec<WindowMapping>,
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
    /// store, creating it if it doesn't exist, in the extent daemon, and maps its DAX window, if it
    /// has one, into `vm`'s guest. `on_lost` stops the VM as the daemon goes.
    pub fn new(
        config: Config,
        queues: u16,
        vm: Arc<dyn WindowVm>,
        on_lost: OnLost,
    ) -> Result<Self, Error> {
        Self::make(config, queues, Making::Boot, vm, on_lost)
    }

    /// The device a snapshot's `state` holds, with `queues` request queues, as many as it had: its
    /// store opened, or forked from the snapshot's point if it doesn't exist; its window in guest
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
        let mut device = Self::make(Config::from(&state.config), queues, making, vm, on_lost)?;
        device.notices.set_frozen(state.frozen);
        device.mappings.clone_from(&snapshot.mappings);
        device.snapshot = Some(snapshot);
        Ok(device)
    }

    /// A device as `config` describes, with `queues` request queues, made as `making` says: its
    /// store opened, or, restored from a snapshot, forked from the snapshot's point if it doesn't
    /// exist; its window in guest memory where the snapshot had it.
    fn make(
        config: Config,
        queues: u16,
        making: Making<'_>,
        vm: Arc<dyn WindowVm>,
        on_lost: OnLost,
    ) -> Result<Self, Error> {
        let mut config_space = ConfigSpace::new_zeroed();
        config_space.version = VIRTIO_EXTENT_VERSION.to_le();
        config_space.num_request_queues = u32::from(queues).to_le();
        config_space.block_size = cladefs::ExtentStore::BLOCK_SIZE.to_le();
        config_space.map_alignment =
            u32::try_from(cladefs::pages::page_size()).expect("a page is small").to_le();
        config_space.max_transfer = MAX_TRANSFER.to_le();
        config_space.max_segments = (u32::from(QUEUE_SIZE) - 2).to_le();
        config_space.max_root = MAX_ROOT.to_le();
        config_space.max_extent = MAX_EXTENT.to_le();
        let tag = config.tag.as_bytes();
        if tag.len() > MAX_TAG_LEN {
            return Err(Error::TagTooLong(config.tag.clone()));
        }
        config_space.tag[..tag.len()].copy_from_slice(tag);

        if config.read_only && matches!(making, Making::Restore { .. }) {
            return Err(Error::ReadOnlyRestored(config.id.clone()));
        }
        // cladefs: a window's faults come through /dev/userfaultfd, which the device opens as the
        // driver activates it: a host that denies it fails the VM now, saying what it lacks, not
        // the guest's activation, after which the VM has no disk (TASK-308).
        if config.dax_window_size.is_some() {
            cladefs::userfaultfd_accessible()
                .map_err(|err| Error::Userfaultfd(config.id.clone(), err))?;
        }
        let store = connect(&config.cladefsd_socket, &config.id, &on_lost)
            .and_then(|daemon| match &making {
                Making::Boot if config.read_only => daemon.open_read_only(&config.path_on_host),
                Making::Boot => daemon.open(&config.path_on_host, config.capacity),
                Making::Restore { snapshot, .. } => daemon.open_forked(
                    &config.path_on_host,
                    &snapshot.point,
                    snapshot.point_branch,
                    config.capacity,
                ),
            })
            .map_err(|err| Error::Store(config.path_on_host.clone(), err))?;
        config_space.branch = store.branch().to_le();
        let notices = Arc::new(Notices::default());
        let info =
            store.info().map_err(|err| Error::Store(config.path_on_host.clone(), err.into()))?;
        notices.resizing.opened(info.capacity);
        let changes = notices.clone();
        store.on_capacity(move |capacity, ask| changes.resizing.changed(capacity, ask));
        hear_stats_asks(&store, &notices);
        let following = Arc::new(Following::default());
        hear_commits(&store, &following);

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
                // cladefs: a writer's window copies on write (backlog doc-13); a follower's copies
                // nothing.
                if !config.read_only {
                    config_space.private_max =
                        private_max(window.len(), config.dax_private_max).to_le();
                }
                Some(window)
            }
            None => None,
        };

        Ok(Self {
            config,
            store: Some(Arc::new(store)),
            window,
            window_created: false,
            prefault: None,
            config_space,
            notices,
            following,
            on_lost,
            snapshot: None,
            mappings: Vec::new(),
        })
    }

    /// Its configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Has `prefault` map the DAX window's pages for the guest as the store maps them, rather
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
    /// its end zeros. cladefs: the store's capacity, and the shrink the guest is asked to take, as
    /// they are now; and a follower's writer's latest commit; and the daemon's latest ask for the
    /// guest's stats.
    pub fn read_config(&self, offset: u64, data: &mut [u8]) {
        let (capacity, asked) = self.notices.resizing.known();
        let config = ConfigSpace {
            capacity: capacity.to_le(),
            asked: asked.to_le(),
            committed: self.following.latest().to_le(),
            stats_asked: self.notices.stats.latest().to_le(),
            ..self.config_space
        };
        config.read(offset, data);
    }

    /// Its store: open, but from a follower's reset until it is activated again.
    fn store(&self) -> &Arc<DaemonExtentStore> {
        self.store.as_ref().expect("the device's store is closed")
    }

    /// The requests of one of its queues, served with its store, while the device is activated:
    /// for a worker taking them off the queue, as [`Device::start_workers`]'s do.
    pub fn requests(&self) -> Requests {
        let window = self.window.as_ref().map_or(0, WindowMemory::len);
        let bounds = Bounds::of(&self.config_space, window);
        Requests::new(self.store().clone(), self.notices.clone(), self.config.cache_type, bounds)
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
        Workers::start(queues, name, || self.requests())
    }

    /// Activates the device, the driver having taken features `acked`, every one offered, and its
    /// transport set up the queues: opens the store anew if a follower's reset let it go, makes the
    /// DAX window, maps again the mappings of the snapshot it was restored from, and has
    /// `interrupt` tell the guest of the daemon's notices. The VMM's workers then serve the queues
    /// ([`Device::requests`]). On a thread under the VMM's seccomp filter, rather than the vCPU's
    /// whose write activates the device: its work, opening the DAX window's userfaultfd and the
    /// like, is the VMM's, not a vCPU's (TASK-50.1).
    pub fn activate(
        &mut self,
        acked: u64,
        interrupt: Arc<dyn ConfigInterrupt>,
    ) -> Result<(), ActivateError> {
        // cladefs: once the queues are, so that an activation failing on them leaves the store
        // closed, to open anew at the next (TASK-79.8), its guest remounting on a new view
        // (doc-4).
        if self.store.is_none() {
            self.reopen().map_err(ActivateError::Reopen)?;
        }
        self.create_dax_window().map_err(ActivateError::Window)?;
        self.map_again().map_err(ActivateError::MapAgain)?;
        self.notices.resizing.set_interrupt(Some(interrupt.clone()));
        self.following.set_interrupt(Some(interrupt.clone()));
        // cladefs: the guest is asked for its stats if its driver takes asks (TASK-111.4).
        let asks = acked & (1 << VIRTIO_EXTENT_F_STATS) != 0;
        self.notices.stats.set_interrupt(asks.then_some(interrupt));
        Ok(())
    }

    /// Resets the device, its VMM's workers stopped: as before the driver activated it. cladefs: a
    /// follower lets go of its store now, not as it is activated again: a store its writer
    /// retired, going with writes uncommitted, opens only once it is gone, for its follower or a
    /// new writer, and the guest may not mount for a while (TASK-79.24). Its guest leaves its
    /// window mapped as it unmounts, for the device's going to unmap: this process would go on
    /// mapping pages the daemon, its connection gone, gives others, so they go first, the daemon's
    /// stage operations made as the reply comes (TASK-79.100). On a thread under the VMM's
    /// seccomp filter, as activation.
    pub fn reset(&mut self) {
        if self.config.read_only {
            if let (Some(store), Some(window), true) =
                (&self.store, &self.window, self.window_created)
                && let Err(err) = store.unmap(0, window.len())
            {
                error!("extent device {}: unmapping its window: {err:#}", self.config.id);
            }
            self.store = None;
        }
        // cladefs: no guest is asked for its stats until a driver activates it again, the store
        // answering meanwhile (TASK-111.4).
        self.notices.stats.set_interrupt(None);
    }

    /// Creates the store's DAX window in the window's host memory, unless the device has no
    /// window, or it exists.
    fn create_dax_window(&mut self) -> Result<(), cladefs::Error> {
        let Some(window) = &self.window else {
            return Ok(());
        };
        if self.window_created {
            return Ok(());
        }
        // cladefs: a writer's copies on write, of `private_max` pages at most; a follower's, of
        // none, copies nothing (backlog doc-13).
        let private_max = u64::from_le(self.config_space.private_max);
        let store = self.store.as_ref().expect("the device's store is closed");
        // SAFETY: The store is dropped before the window, as the fields are in that order, and
        // replaced only once its window's connection went (`reset`, `reopen`).
        unsafe { window.create(store, self.prefault.clone(), private_max) }?;
        self.window_created = true;
        Ok(())
    }

    /// cladefs: opens a follower's store anew, as of its writer's latest commit, with a window of
    /// its own, created as the driver activates the device: after a reset, its guest remounting
    /// on a new view, as it does once it was cut off (`ESTALE`). The old connection went as it
    /// reset ([`Device::reset`]), with its window's faults, before the new one creates its window
    /// in the same memory, which it left inaccessible meanwhile (TASK-79.8).
    fn reopen(&mut self) -> Result<(), cladefs::Error> {
        let store = connect(&self.config.cladefsd_socket, &self.config.id, &self.on_lost)?
            .open_read_only(&self.config.path_on_host)?;
        self.following = Arc::new(Following::default());
        hear_commits(&store, &self.following);
        hear_stats_asks(&store, &self.notices);
        self.config_space.branch = store.branch().to_le();
        self.store = Some(Arc::new(store));
        self.window_created = false;
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
        self.store().map_many(&mappings)?;
        info!(
            "extent device {}: the snapshot's {} window mappings mapped again in {:?}",
            self.config.id,
            mappings.len(),
            start.elapsed()
        );
        Ok(())
    }

    /// cladefs: takes a snapshot of the device's store, for a snapshot of the VM, paused, whose
    /// guest's filesystem is frozen, with nothing open (TASK-39.1): `quiesce` has the VMM's
    /// workers serve the requests they took and stop, as they do for the snapshot, so that none
    /// changes the window meanwhile; then the store is forked to `point`, and the window's mappings
    /// recorded, which [`Device::state`] says.
    pub fn take_snapshot(
        &mut self,
        point: &Path,
        quiesce: impl FnOnce(),
    ) -> Result<(), SnapshotError> {
        let id = self.config.id.clone();
        if self.config.read_only {
            return Err(SnapshotError::ReadOnly(id));
        }
        if !self.notices.frozen() {
            return Err(SnapshotError::NotFrozen(id));
        }
        quiesce();
        // cladefs: the point's branch, the extent daemon's to name, its ID one no other branch of its
        // bucket has (TASK-364).
        let branch = self
            .store()
            .fork_quiescent(point)
            .map_err(|errno| SnapshotError::Fork(id.clone(), point.display().to_string(), errno))?;
        let mappings =
            self.store().window_mappings().map_err(|errno| SnapshotError::Mappings(id, errno))?;
        self.snapshot = Some(Snapshot { point: point.to_owned(), point_branch: branch, mappings });
        Ok(())
    }

    /// The snapshot taken of the device's store ([`Device::take_snapshot`]), or restored from.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    /// What a snapshot of its VM holds of the device, once [`Device::take_snapshot`] took the
    /// snapshot of its store, which the VMM has it do first.
    pub fn state(&self) -> State {
        let snapshot = self
            .snapshot()
            .expect("an extent device's store is snapshotted before the device is saved");
        State {
            config: ConfigState::from(&self.config),
            window_addr: self.window.as_ref().map(WindowMemory::guest_addr),
            point: snapshot.point.clone(),
            point_branch: snapshot.point_branch,
            mappings: snapshot.mappings.iter().map(MappingState::from).collect(),
            frozen: self.notices.frozen(),
        }
    }
}

/// cladefs: a connection to the extent daemon listening at `socket`, the device `id`'s, which has
/// `on_lost` stop the VM once the daemon goes: the guest's disk is gone then, and the VM stops at
/// once, rather than going on with every request failing (TASK-50.2). Said before the store opens
/// over it, so that a daemon going as it opens, as the VM is created, stops it so too, rather than
/// as a store that failed to open (TASK-232).
fn connect(
    socket: impl AsRef<Path>,
    id: &str,
    on_lost: &OnLost,
) -> Result<DaemonConnection, cladefs::Error> {
    let daemon = DaemonConnection::connect(socket)?;
    let (id, on_lost) = (id.to_owned(), on_lost.clone());
    daemon.on_lost(move || {
        error!("extent device {id}: the extent daemon went");
        on_lost();
    });
    Ok(daemon)
}

/// cladefs: has `following` hear of the commits of the writer of `store`, a follower's, from the
/// one its view is as of.
fn hear_commits(store: &DaemonExtentStore, following: &Arc<Following>) {
    let heard = following.clone();
    following.committed(store.on_committed(move |commit| heard.committed(commit)));
}

/// cladefs: has the daemon's asks for the stats of `store`'s VM process and guest ask the guest,
/// through `notices`, while its driver takes asks; otherwise the store answers at once, with none
/// of the guest's (TASK-111.4).
fn hear_stats_asks(store: &DaemonExtentStore, notices: &Arc<Notices>) {
    let notices = notices.clone();
    store.on_stats_asked(move |ask| notices.stats.ask(ask));
}
