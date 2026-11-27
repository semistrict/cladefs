// Copyright 2026 The cladefs Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! cladefs's extent store device (doc-4, virtio device type 0x4658) on libmachine's virtio-PCI
//! transport: libmachine's adapter of `virtio_extent`, which holds the device itself (its
//! protocol, requests, notices, DAX window and snapshots) once for every VMM hosting it
//! (`crates/virtio_extent/README.md` lists what an adapter does; Firecracker's,
//! `vendor/firecracker/src/vmm/src/devices/virtio/extent`, is the other).
//!
//! - **Queues:** a request queue of `QUEUE_SIZE` per vCPU, each with an MSI-X vector of its own.
//!   virtio_extent's workers serve them, a thread each, not libmachine's device thread
//!   (`VirtioDevice::serves_its_queues`): the transport hands the device copies of the queues the
//!   driver set up as it activates the device, with their notification events and the eventfds of
//!   their vectors, which the workers signal from the threads completing requests (hotbed
//!   question 572).
//! - **The DAX window:** a 64-bit BAR of the device's own, allocated before the transports' BARs
//!   and placed by the device (`PlacedShm`), which virtio_extent reserves ([`ExtentVm`]). On KVM
//!   it adds its memory slot (`MemorySlots`), into which cladefs maps the window's pages from the
//!   extent daemon's memory, and prefaults them with `KVM_PRE_FAULT_MEMORY` on a vCPU of its own
//!   (`virtio_extent::kvm`). On Hypervisor.framework libmachine maps the pages into the guest at
//!   the BAR itself (`hv_vm_map`) as cladefs's stage says, and the vCPUs hand it the guest's
//!   aborts on the window (`virtio_extent::window_fault`, board 13069). The BAR never moves
//!   (hotbed question 571).
//! - **Stopping the machine:** as the extent daemon goes, or the device can't be activated, the
//!   machine stops for good with `Error::ExtentStore` (`StopShared::fail`); the vCPUs stop it so
//!   too when a guest's access to the window keeps failing (`virtio_extent::vcpu`; hotbed question
//!   573).
//! - **Snapshots** (board 13139), as doc-4 has them and Firecracker's adapter takes them: the
//!   machine's snapshot pauses the device's workers, their queues back in the transport where
//!   they left them (`VirtioDevice::pause_queues`), then has virtio_extent take the device's
//!   snapshot (`Device::take_snapshot_with`): versioned, refused unless the guest's filesystem is
//!   frozen, a version of the cell made and the window's mappings recorded, not its memory; or for
//!   a migration, the cell handed over whole, refused while it has uncommitted open extents.
//!   Either refuses while the window holds private pages. Its state goes into the snapshot's
//!   header, so that a restore makes the device from it ([`ExtentDevice::new`] given it): its
//!   cell forked from the version (or reopened), its window at the same BAR, mapped again as the
//!   driver activates it as it was. The transport's state holds the device's protocol version,
//!   which a restore checks. A snapshot that fails after the device's drops its version; the
//!   workers start again either way.

use std::os::fd::{AsRawFd, BorrowedFd};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::ptr::NonNull;
use std::sync::{Arc, Weak};

use virtio_extent::protocol::{
    ConfigSpace, QUEUE_SIZE, VIRTIO_EXTENT_VERSION, VIRTIO_ID_EXTENT, check_acked, check_version,
};
use virtio_extent::{BoxError, Buffers, Popped, Workers};
use vm_memory::{GuestMemoryBackend, GuestMemoryMmap};

use super::queue::DeviceQueue;
use super::{
    DeviceState, OwnQueues, PlacedShm, VIRTIO_F_VERSION_1, VIRTIO_RING_F_EVENT_IDX, VirtioDevice,
};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::backend::MemorySlots;
use crate::devices::pci::BarAllocator;
use crate::error::{Error, Result};
use crate::extent::{ExtentCache, ExtentDaemon, ExtentStore};
use crate::stop::{Failure, StopShared};
use crate::sys::EventFd;

/// The device's type, as its PCI IDs say it (`pci_ids`).
const DEVICE_TYPE: u16 = VIRTIO_ID_EXTENT as u16;
const _: () = assert!(VIRTIO_ID_EXTENT == DEVICE_TYPE as u32);

/// What stops the machine for good, saying why: its [`StopShared`], held weakly, as the machine
/// owns its devices.
#[derive(Clone)]
pub(crate) struct Failing(Weak<StopShared>);

impl Failing {
    pub(crate) fn new(stop: &Arc<StopShared>) -> Self {
        Self(Arc::downgrade(stop))
    }

    fn fail(&self, why: Failure) {
        if let Some(stop) = self.0.upgrade() {
            stop.fail(why);
        }
    }

    /// Why the machine was stopped for good, if it was.
    fn failure(&self) -> Option<Error> {
        self.0.upgrade().and_then(|stop| stop.failure())
    }
}

/// An extent store device on libmachine's transport.
pub(crate) struct ExtentDevice {
    // The workers before the device: they serve its requests over its cell.
    workers: Option<Workers<ExtentQueue>>,
    device: virtio_extent::Device,
    queue_sizes: Vec<u16>,
    /// The DAX window, if the device has one.
    placed: Option<PlacedShm>,
    failing: Failing,
}

impl ExtentDevice {
    /// The device `store` describes, with `queues` request queues, its cell opened on `daemon`,
    /// its DAX window (if it has one) placed from `bars`' 64-bit window and mapped by `window_vm`,
    /// given the window's BAR, prefaulted by `prefault` if given; `failing` stops the machine.
    /// Restored from a snapshot, the device is made from what it `saved` of itself, its cell then
    /// `store`'s (forked from the snapshot's version, or reopened), the rest of `store` as the
    /// snapshot's (`Snapshot::check`), its window where its BAR was.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: &ExtentStore,
        daemon: &ExtentDaemon,
        queues: u16,
        bars: &mut BarAllocator,
        window_vm: impl FnOnce(Option<u64>) -> Arc<dyn virtio_extent::WindowVm>,
        prefault: Option<Arc<dyn virtio_extent::PrefaultGuestMemory>>,
        failing: Failing,
        saved: Option<&virtio_extent::persist::State>,
    ) -> Result<Self> {
        let id = store.id.clone();
        let invalid =
            |why: String| Error::InvalidConfig(format!("extent store device {id}: {why}"));
        let config = virtio_extent::Config {
            id: id.clone(),
            tag: store.tag.clone(),
            path_on_host: store.cell.path_on_host().map_err(invalid)?,
            capacity: store.capacity,
            dax_window_size: store.dax_window_size,
            dax_private_max_bytes: store.dax_private_max_bytes,
            // The connection is the daemon's (`ExtentDaemon::connect`); its socket, if it listens
            // at one, is where a cell let go of is opened again (`release_cell`): a daemon in
            // this process has none, and its devices' cells are never let go of.
            cladefsd_socket: (daemon.socket())
                .map(|socket| socket.display().to_string())
                .unwrap_or_default(),
            cache_type: match store.cache {
                ExtentCache::Writeback => virtio_extent::CacheType::Writeback,
                ExtentCache::Unsafe => virtio_extent::CacheType::Unsafe,
            },
        };
        config
            .validate()
            .map_err(|err| Error::InvalidConfig(format!("extent store device {id}: {err}")))?;
        if queues == 0 {
            return Err(Error::InvalidConfig(format!(
                "extent store device {id}: no request queue"
            )));
        }
        // The window's BAR, a power of two as BARs are, before any transport's.
        let window = match store.dax_window_size {
            Some(len) => {
                let bar_size = len.next_power_of_two();
                Some((bars.allocate(bar_size, true)?, bar_size))
            }
            None => None,
        };
        let vm = window_vm(window.map(|(at, _)| at));
        let connection = daemon.connect().map_err(|err| {
            Error::ExtentStore(format!("{id}: connecting to the extent daemon: {err:#}"))
        })?;
        let on_lost: virtio_extent::OnLost = {
            let (id, failing) = (id.clone(), failing.clone());
            Arc::new(move || {
                failing.fail(Failure::DaemonLost(format!("{id}: the extent daemon went")))
            })
        };
        let device = match saved {
            None => virtio_extent::Device::connected(config, connection, queues, vm, on_lost),
            Some(saved) => {
                if saved.window_addr != window.map(|(at, _)| at) {
                    return Err(Error::Snapshot(format!(
                        "extent store device {id}: the snapshot's DAX window was at {:x?}, this \
                         machine's BAR for it at {:x?}",
                        saved.window_addr,
                        window.map(|(at, _)| at)
                    )));
                }
                let mut state = saved.clone();
                state.config = virtio_extent::persist::ConfigState::from(&config);
                virtio_extent::Device::restore_connected(&state, connection, queues, vm, on_lost)
            }
        };
        // A daemon going as the cell opened stopped the machine first (`on_lost`): that is the
        // failure, not the open's, as Firecracker's is (`CladefsdLost`, TASK-232).
        let mut device = device.map_err(|err| {
            failing.failure().unwrap_or_else(|| Error::ExtentStore(format!("{id}: {err}")))
        })?;
        if let Some(prefault) = prefault {
            device.set_prefault(prefault);
        }
        let placed = match (device.shm_region(), window) {
            (Some(region), Some((at, bar_size))) => {
                debug_assert_eq!(region.guest_addr, at, "the window where its BAR was allocated");
                Some(PlacedShm { id: region.id, guest_addr: at, len: region.len, bar_size })
            }
            (None, None) => None,
            _ => unreachable!("a device has a window as its configuration says"),
        };
        Ok(Self {
            workers: None,
            device,
            queue_sizes: vec![QUEUE_SIZE; usize::from(queues)],
            placed,
            failing,
        })
    }

    /// The features the transport offers, its own among them, as a driver must take every one.
    fn offered(&self) -> u64 {
        self.device.features() | 1 << VIRTIO_F_VERSION_1 | 1 << VIRTIO_RING_F_EVENT_IDX
    }

    /// Activates the device and starts its workers on `own`'s ready queues: all of them, or
    /// none.
    fn start(&mut self, features: u64, mut own: OwnQueues<'_>) -> std::result::Result<(), String> {
        // The transport activates a device once until a reset, which stops its workers.
        if self.workers.is_some() {
            return Err("it is active already".into());
        }
        check_acked(self.offered(), features).map_err(|refused| refused.to_string())?;
        let config = Arc::new(Vector(own.config_vector));
        let mut queues = Vec::new();
        let mut kicks = Vec::new();
        for (index, queue) in own.queues.iter().enumerate().filter(|(_, queue)| queue.ready()) {
            let notify = &own.notify[index];
            // SAFETY: The notification event's descriptor (an eventfd, or a pipe's read end on
            // macOS), open while the transport holds the event, as it does here.
            let event = unsafe { BorrowedFd::borrow_raw(notify.as_raw_fd()) }
                .try_clone_to_owned()
                .map_err(|err| err.to_string())?;
            kicks.push(notify.try_clone().map_err(|err| err.to_string())?);
            queues.push(virtio_extent::WorkerQueue {
                index: u16::try_from(index).expect("the transport's queues are few"),
                queue: ExtentQueue { queue: queue.duplicate(), mem: own.mem.clone() },
                // The worker's own copy of the descriptor.
                event,
                interrupt: Arc::new(Vector(own.queue_vectors[index].take())),
            });
        }
        // As README.md asks, the work of the VMM's on a thread of cladefs's spawner (which
        // libmachine sets none of), not on the vCPU's whose write activates the device.
        let (device, workers) = (&mut self.device, &mut self.workers);
        let activated = cladefs::thread::run("lm_extent_activate", || {
            device.activate(features, config).map_err(|err| err.to_string())?;
            *workers = Some(
                device
                    .start_workers(queues, "lm_extent_q")
                    .map_err(|err| format!("starting its workers: {err}"))?,
            );
            Ok::<(), String>(())
        });
        activated.map_err(|err| format!("starting its activation: {err}"))??;
        // What the driver made available before DRIVER_OK, which no notification told of.
        for kick in kicks {
            kick.write(1).map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    fn stop_workers(&mut self) {
        if let Some(workers) = self.workers.take() {
            workers.stop();
        }
    }
}

impl Drop for ExtentDevice {
    fn drop(&mut self) {
        self.stop_workers();
    }
}

impl VirtioDevice for ExtentDevice {
    fn device_type(&self) -> u16 {
        DEVICE_TYPE
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn features(&self) -> u64 {
        self.device.features()
    }

    fn config_size(&self) -> usize {
        size_of::<ConfigSpace>()
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) {
        self.device.read_config(offset as u64, data);
    }

    // The driver writes nothing of the configuration space: what it would is ignored.

    fn placed_shm(&self) -> Option<PlacedShm> {
        self.placed
    }

    fn serves_its_queues(&self) -> bool {
        true
    }

    fn activate_queues(&mut self, features: u64, own: OwnQueues<'_>) -> Result<()> {
        self.start(features, own).map_err(|why| {
            let why = format!("{}: activating it: {why}", self.device.config().id);
            // The guest's disk is gone: the machine stops, saying why, rather than run on
            // without it.
            self.failing.fail(Failure::Other(why.clone()));
            Error::ExtentStore(why)
        })
    }

    fn reset(&mut self) {
        // As activation, on a thread of cladefs's spawner, not the vCPU's whose write resets it.
        let (device, workers) = (&mut self.device, &mut self.workers);
        let reset = cladefs::thread::run("lm_extent_reset", || {
            if let Some(workers) = workers.take() {
                workers.stop();
            }
            device.reset();
        });
        if let Err(err) = reset {
            let id = &self.device.config().id;
            self.failing.fail(Failure::Other(format!("{id}: starting its reset: {err}")));
        }
    }

    // Its queues are its workers': the device thread never hands it one.
    fn process_queue(&mut self, _: usize, _: &mut [DeviceQueue], _: &GuestMemoryMmap) {}

    fn pause_queues(&mut self) -> Vec<(usize, DeviceQueue)> {
        let stopped = self.workers.take().map(Workers::stop).unwrap_or_default();
        stopped.into_iter().map(|(index, queue)| (usize::from(index), queue.queue)).collect()
    }

    fn take_snapshot(
        &mut self,
        mode: virtio_extent::SnapshotMode,
    ) -> Result<Option<virtio_extent::persist::State>> {
        // Its workers are paused already (`pause_queues`): nothing more to quiesce.
        self.device
            .take_snapshot_with(mode, || {})
            .map_err(|err| Error::Snapshot(err.to_string()))?;
        Ok(Some(self.device.state()))
    }

    fn drop_snapshot(&mut self) {
        self.device.drop_snapshot();
    }

    fn release_cell(&mut self) -> Result<()> {
        // Its workers are paused already (`pause_queues`): nothing more to quiesce.
        self.device.release_cell(|| {}).map_err(|err| Error::ExtentStore(err.to_string()))
    }

    fn reopen_cell(&mut self) -> Result<()> {
        self.device.reopen_cell().map_err(|err| Error::ExtentStore(err.to_string()))
    }

    /// The device's protocol version, which its guest's driver checked as it probed it: the rest
    /// of its state is in the snapshot's header (`take_snapshot`), as a restore makes the device
    /// from it.
    fn save(&self) -> Result<DeviceState> {
        Ok(DeviceState { version: VIRTIO_EXTENT_VERSION, data: Vec::new() })
    }

    fn restore(&mut self, state: &DeviceState) -> Result<()> {
        check_version(state.version).map_err(|err| Error::Snapshot(err.to_string()))?;
        if !state.data.is_empty() {
            return Err(Error::Snapshot(format!(
                "the snapshot's extent store device {} has state of its own, which this one \
                 does not know",
                self.device.config().id
            )));
        }
        Ok(())
    }
}

/// A request queue of the device's, a copy of the transport's, in its guest's memory.
pub(crate) struct ExtentQueue {
    queue: DeviceQueue,
    mem: GuestMemoryMmap,
}

impl virtio_extent::RequestQueue for ExtentQueue {
    // With EVENT_IDX, `DeviceQueue::pop` asks the driver to notify again once it finds the queue
    // empty, and looks once more.
    fn pop(&mut self) -> std::result::Result<Option<Popped>, BoxError> {
        let Some(chain) = self.queue.pop(&self.mem) else {
            return Ok(None);
        };
        let index = chain.head_index();
        let mut buffers = Buffers::new();
        let made =
            chain.into_iter().try_for_each(|descriptor| -> std::result::Result<(), BoxError> {
                let len = descriptor.len() as usize;
                let slice = self.mem.get_slice(descriptor.addr(), len)?;
                let ptr = slice.ptr_guard_mut().as_ptr();
                if descriptor.is_write_only() {
                    // SAFETY: Guest memory, mapped as long as the machine, whose devices the workers
                    // stop before it goes (`ExtentDevice`'s drop), and so while the requests they took
                    // are served (the workers wait for them as they stop), which only the request
                    // writes, as the driver gave it the device to write.
                    unsafe { buffers.push_writable(ptr, len) }?;
                } else {
                    // SAFETY: As above, which the request reads.
                    unsafe { buffers.push_readable(ptr, len) };
                }
                Ok(())
            });
        Ok(Some(Popped { index, buffers: made.map(|()| buffers) }))
    }

    fn complete(&mut self, index: u16, len: u32) -> std::result::Result<bool, BoxError> {
        if !self.queue.add_used(&self.mem, index, len) {
            return Err("the used ring is not in guest memory".into());
        }
        Ok(self.queue.needs_notification(&self.mem))
    }
}

/// An MSI-X vector's eventfd (`Msix::vector_event`): a queue's, or the configuration change
/// interrupt's; none for one the driver gave no vector, which interrupts nothing.
struct Vector(Option<EventFd>);

impl Vector {
    fn signal(&self) -> std::result::Result<(), BoxError> {
        match &self.0 {
            Some(event) => Ok(event.write(1)?),
            None => Ok(()),
        }
    }
}

impl virtio_extent::QueueInterrupt for Vector {
    fn notify(&self) -> std::result::Result<(), BoxError> {
        self.signal()
    }
}

impl virtio_extent::ConfigInterrupt for Vector {
    fn config_changed(&self) -> std::result::Result<(), BoxError> {
        self.signal()
    }
}

/// What only libmachine can do for the device's DAX window (`virtio_extent::WindowVm`): its guest
/// physical addresses, the 64-bit BAR allocated for it ([`ExtentDevice::new`]), and the KVM memory
/// slot mapping its host memory there.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(crate) struct ExtentVm {
    pub(crate) slots: MemorySlots,
    /// The window's BAR's address.
    pub(crate) at: Option<u64>,
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
impl virtio_extent::WindowVm for ExtentVm {
    fn reserve(&self, _len: u64, at: Option<u64>) -> std::result::Result<u64, BoxError> {
        match (at, self.at) {
            // A snapshot's window, where its BAR was, as it is again (`ExtentDevice::new`).
            (Some(at), Some(bar)) if at == bar => Ok(bar),
            (Some(at), bar) => {
                Err(format!("the snapshot's window at {at:#x}, its BAR at {bar:x?}").into())
            }
            (None, Some(bar)) => Ok(bar),
            (None, None) => Err("a window the device's configuration has none of".into()),
        }
    }

    // The BAR is the machine's for its life, as every BAR is: nothing to release.
    fn release(&self, _start: u64, _len: u64) {}

    fn add_slot(
        &self,
        guest_addr: u64,
        len: u64,
        host: NonNull<u8>,
    ) -> std::result::Result<u32, BoxError> {
        // SAFETY: The window's host memory, which virtio_extent keeps mapped until it removes the
        // slot (its `WindowMemory`'s fields' drop order).
        Ok(unsafe { self.slots.map_raw(guest_addr, host.as_ptr(), len) }?)
    }

    fn remove_slot(&self, slot: u32) {
        // virtio_extent unmaps the window's host memory once this returns: one KVM still mapped
        // would let the guest reach whatever the host maps there next. A machine with the window
        // mapped is never suspended (the suspension refuses device memory), so its VM is live,
        // and KVM deletes a live slot; if it ever can't, nothing is safe but to stop here.
        if let Err(err) = self.slots.unmap_raw(slot) {
            eprintln!("libmachine: cannot remove the DAX window's memory slot {slot}: {err}");
            std::process::abort();
        }
    }
}

/// What only libmachine can do for the device's DAX window on Hypervisor.framework
/// (`virtio_extent::WindowVm`): its guest physical addresses, the 64-bit BAR allocated for it
/// ([`ExtentDevice::new`]), into which the window's pages are mapped as cladefs's stage says
/// (`hv_vm_map`, board 13069).
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) struct ExtentVm {
    /// The VM, which maps the window's pages.
    pub(crate) vm: Arc<crate::backend::hvf::Shared>,
    /// The window's BAR's address.
    pub(crate) at: Option<u64>,
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
impl virtio_extent::WindowVm for ExtentVm {
    fn reserve(&self, _len: u64, at: Option<u64>) -> std::result::Result<u64, BoxError> {
        match (at, self.at) {
            // A snapshot's window, where its BAR was, as it is again (`ExtentDevice::new`).
            (Some(at), Some(bar)) if at == bar => Ok(bar),
            (Some(at), bar) => {
                Err(format!("the snapshot's window at {at:#x}, its BAR at {bar:x?}").into())
            }
            (None, Some(bar)) => Ok(bar),
            (None, None) => Err("a window the device's configuration has none of".into()),
        }
    }

    // The BAR is the machine's for its life, as every BAR is: nothing to release.
    fn release(&self, _start: u64, _len: u64) {}

    unsafe fn map(&self, guest_addr: u64, host: std::ptr::NonNull<u8>, len: u64, write: bool) {
        // SAFETY: As the stage promises: the memory stays mapped until it unmaps it.
        unsafe { self.vm.map_window(guest_addr, host, len, write) };
    }

    fn unmap(&self, guest_addr: u64, len: u64) {
        self.vm.unmap_window(guest_addr, len);
    }
}
