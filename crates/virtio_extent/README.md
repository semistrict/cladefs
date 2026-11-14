# virtio_extent

fxdax's virtio extent-store device (backlog doc-4, device type `0x4658`) but its virtio
transport: one copy of it, which every VMM hosting the device serves, so that its protocol,
which is versioned and never migrates, can't drift between them. Its VMMs are fxdax's
Firecracker fork (`vendor/firecracker`, `src/vmm/src/devices/virtio/extent`, the reference
adapter) and libmachine.

## What the crate does

- **Protocol** (`protocol`): the requests, flags and features, the structures the driver and the
  device exchange, the configuration space, and `VIRTIO_EXTENT_VERSION`, with the checks of a
  driver's features (`check_acked`) and of a snapshot's version (`check_version`).
- **Configuration** (`Config`): a device's, as a VMM's user gives it, serializable, with its
  validation and the features it makes.
- **The device** (`Device`, Linux): opens its store in fxdax's extent daemon, builds and reads
  its configuration space, hears the daemon's notices (capacity, a follower's commits, asks for
  the guest's stats) and has the VMM raise a configuration change interrupt for them, makes and
  creates the DAX window, and takes snapshots of the store and restores from them.
- **Requests** (`Buffers`, `handle`, `Requests`): a request's buffers, built from its
  descriptors' guest memory in their order, served over any `Store` (fxdax's
  `RemoteExtentStore` on Linux, a fake in tests); `Requests` serves a queue's on the threads
  receiving the daemon's replies, counting those in flight and what each type took.
- **Snapshots** (`persist`): the device's state as plain serializable values, and doc-4's
  freeze, point and fork protocol (`Device::take_snapshot`, `Device::restore`).

Nothing in its public API is Firecracker's or KVM's, and it depends on no VMM's crates, nor on
vm-memory: guest memory reaches it as host addresses and lengths.

## What a VMM's adapter does

Its transport and queues:

- Offers the device as a virtio-PCI device of type `0x4658` (MMIO can't describe the window),
  with a request queue of `QUEUE_SIZE` per vCPU, each with an MSI-X vector of its own, and
  features `VIRTIO_F_VERSION_1`, its own transport's (`VIRTIO_RING_F_EVENT_IDX` if its queues
  suppress notifications) and `Device::features`. A driver must take every one offered:
  `check_acked` refuses its activation otherwise.
- Reads the configuration space with `Device::read_config`; ignores the driver's writes to it.
- As the driver activates the device: checks the features, sets up the queues the driver made
  ready, calls `Device::activate` with a `ConfigInterrupt` that raises the transport's
  configuration change interrupt, then starts its workers.
- Its workers take requests off the queues, turn each descriptor chain into `Buffers`
  (`push_readable`/`push_writable`, in descriptor order, marking writable guest memory dirty if
  it tracks it), and serve them with a `Requests` of the device's (`Device::requests`, one per
  queue). `Requests::serve` calls back with the length written; the worker puts the request on
  the used ring (with event idx, as its queue does) and signals the queue's interrupt. Before
  stopping, a worker waits for its requests in flight (`Requests::wait_idle`).
- On a reset: stops its workers, then `Device::reset`.
- Does the activation and the reset on threads under the VMM's own seccomp filter, not a
  vCPU's (Firecracker uses `fxdax::thread`'s spawner). The daemon's client threads, and those
  serving the window's faults, are fxdax's; the adapter chooses which threads take requests.

The DAX window:

- Implements `WindowVm`: reserves the window's guest physical range (a 64-bit BAR of the
  device's own, sized and aligned as BARs are, and at a given address on a restore), and adds
  and removes a KVM memory slot for its host memory while the VM runs.
- Describes the window to the driver as shared memory region `DAX_WINDOW_SHM_ID` with
  `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` capabilities on that BAR (`Device::shm_region`).
- Gives the device a `PrefaultGuestMemory` if it can (`Device::set_prefault`): KVM's
  `KVM_PRE_FAULT_MEMORY`, on a vCPU of its own that never runs (Firecracker's `KvmPrefault`).

Its vCPUs:

- Call `fxdax::guest_thread_started` on each vCPU thread before it first runs.
- Retry `KVM_RUN` when it fails with `EFAULT` while fxdax remaps the window
  (`fxdax::AccessFailures`), and stop the VM when the failures persist, telling what it can of
  them (`dax_window_at`, `fxdax::describe_windows`; `dax_window_at` assumes one VM per process,
  as Firecracker's are). Firecracker exits with status 159.
- Stop the VM when a store's private pages pass `private_max` (the guest's store is refused),
  and when the extent daemon goes (`OnLost`, which `Device::new` takes; Firecracker exits with
  status 158).

Snapshots:

- To snapshot, with the VM paused and the guest's filesystem frozen: `Device::take_snapshot`,
  whose callback stops the workers once their requests are served; then saves
  `VIRTIO_EXTENT_VERSION`, its transport's state and `Device::state` in its own format.
- To restore: refuses a snapshot whose version `check_version` refuses, restores its transport's
  state, and makes the device with `Device::restore`, whose window is where the snapshot had it.

## Tests

`cargo nextest run -p virtio_extent`: the protocol, configuration, notices and request handling
over a fake store in fake guest memory, everywhere; on Linux, `tests/daemon.rs` serves requests
from a daemon of the test's, and makes, activates, snapshots and restores devices over one.
