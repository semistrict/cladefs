# The extent device as a vhost-user backend: guest memory the daemon reaches

Research for board 14378 (2026-11-27). The question: can cladefsd serve a guest's extent-device
virtqueues itself, as a vhost-user backend, taking the VMM out of every request's path, without
losing the performance guest RAM has today? And would that let our Firecracker fork shrink to what
upstream could take?

Claims marked **derived** follow from the sources but were not measured here. Nothing below was
measured on our hosts yet: see "Measurements to make".

## Why vhost-user

Today every non-DAX request goes guest → Firecracker's extent device (`virtio_extent`, carried in our
fork) → `CellClient` → the daemon's socket, and back: two extra wakeups and a copy per request, and a
fork whose whole extent device (window, snapshots, freeze) no upstream will take. As a vhost-user
backend, the device lives in cladefsd; the VMM needs only generic features any vhost-user device
wants, which QEMU and Cloud Hypervisor have and upstream Firecracker has open work on:

- a generic vhost-user device frontend (Firecracker has vhost-user block only, in developer preview);
- shared memory regions and `VHOST_USER_BACKEND_SHMEM_MAP` for the DAX window (in the vhost-user spec
  and QEMU since 2026; one mapping per message, so our batched maps would be a spec extension);
- snapshot and restore of vhost-user devices: refused by Firecracker today
  ("vhost-user-block does not support snapshotting yet"); Cloud Hypervisor added it in 2026 with
  `SET_DEVICE_STATE_FD`;
- device reset (Firecracker issue #6277: the backend keeps guest memory after a reset).

The cost is the backend reaching guest memory: vhost-user hands the backend every guest RAM region
as a memfd, which it maps.

## memfd guest RAM against private anonymous RAM

1. **Huge pages.** `transparent_hugepage/shmem_enabled` defaults to `never`, and it governs memfd and
   `MAP_SHARED|MAP_ANONYMOUS` alike: `MADV_HUGEPAGE` on a memfd does nothing unless it is `advise`,
   `always`, `within_size` or `force` ([kernel THP docs][thp]). Anonymous memory gets huge pages under
   the usual `enabled=madvise|always`. One microVM project measured 131,072 4 KiB nested EPT faults
   (~1.8 s) touching a 512 MiB working set after restore with memfd RAM under `never`, against 256
   2 MiB faults once `shmem_enabled=within_size` ([substrate PR 1850][substrate]); another, boot to
   init 464 against 414 (`never` against `always`, [hvi-vmm 401][hvi]). Cloud Hypervisor's shared
   memory gets huge pages only if the host's `shmem_enabled` allows ([Cloud Hypervisor memory][ch]).
   Settable at boot: `transparent_hugepage_shmem=`.
2. **Faults.** Firecracker measured shared-memory faults up to 24% slower than private ones (atomic
   page-state updates), and backs guest RAM with a memfd only when a vhost-user device is configured
   for that reason (its vhost-user block doc; [PR 6233][fc6233] background, #4498).
3. **Reclaim.** Freeing a memfd page takes `fallocate(PUNCH_HOLE)` or `MADV_REMOVE`, not
   `MADV_DONTNEED`; Firecracker added balloon and virtio-mem reclaim for memfd RAM in #6237. KSM
   merges no shmem pages.
4. **Restored VMs.** Firecracker gives a snapshot-restored VM private memory, so a vhost-user device
   can't be used or hot-plugged there (CHANGELOG #6204); PR 6233 aligns the memfd's layout with the
   snapshot file's, groundwork for sharing it ([PR 6233][fc6233]).

**What this means for cladefs (derived).** Our restored and forked VMs (most of them) already run on
4 KiB pages: their RAM is a userfaultfd-registered mapping with the daemon's memfd pages installed
read-only, 4 KiB at a time, and private copies where the guest writes, which already live in a
per-window memfd (`cladefs-private-pages`, `PrivateMemory` in `cladefs_core::dax::uffd`, mapped shared
at the window's offsets; review 174 corrected an earlier "anonymous copies" here). So the shmem cost of
private copies is paid today. A
freshly booted VM would lose its huge pages unless the host sets `shmem_enabled` to `within_size` or
`advise`; cladefs owns its production hosts' kernel configuration (AGENTS.md), so that is a host
setting, which hotbed's test workspaces would need too.

The mechanism the daemon would serve a restored VM's buffers through exists: shmem minor faults,
resolved with `UFFDIO_CONTINUE` from a page another mapping filled ([shmem minor faults][minor]),
since Linux 5.13/5.14; Firecracker's own guest_memfd restore work uses the same
([Kalyazin][kalyazin]).

## A way around shared guest RAM: a buffer region the device owns

We write the guest's driver (`guest/linux`, `fs/cladefs`), so the guest need not hand the device
pointers into its RAM at all. The device could offer a second shared memory region (region 1, beside
the DAX window's 0): memory the daemon makes and owns, budgeted as its own, mapped into the guest by
the VMM as `SHMEM_MAP` maps the window. The driver copies a write's data into region 1 and a read's
out of it, and its descriptors name region 1 alone, which the daemon refuses any descriptor outside
of. Guest RAM stays private anonymous memory, with its huge pages and today's restore path untouched;
vhost-user's memory table holds no guest RAM at all.

The copies are as many as today's (the VMM copies into the transfer area now; the guest would copy
into region 1), but the VMM's hop and its two wakeups go. Confidential guests already work so
(swiotlb bounce buffers, `restricted-dma-pool`). What it costs: a doc-4 change (descriptors into a
region, not RAM: a new protocol version, and a guest kernel roll), region 1's memory in the daemon,
and requests bounded by its size.

## The virtqueues are in guest RAM

The buffer region above keeps a request's data out of guest RAM, but not its virtqueue: the guest's
virtio core allocates the descriptor table and both rings in guest RAM, and a vhost-user backend
reads them there (`VHOST_USER_SET_VRING_ADDR`, translated through the memory table). So the daemon
must reach guest RAM whatever carries the data, and the buffer region buys nothing on its own.

## Decision and plan (v3)

**The goal** (the user, 2026-11-27): an extent device our Firecracker fork can upstream, without making
guests much slower. Faster is welcome; it isn't the point. Decided: cladefsd becomes the extent
device's vhost-user backend (hotbed question 591).

**How v3 came to be.** Review 174 (Fable 5.2, 12 findings) found the first plan couldn't work: the DAX
window doesn't fit `SHMEM_MAP`, and mapping guest RAM in the daemon couldn't write a memory cell's
shared pages. v2 answered with two extensions, `SHMEM_LAZY` and `MEM_REMOTE`; review 175 (11 findings)
confirmed the kernel mechanism of `MEM_REMOTE` but found its peer identity, the table's invalidation
order and its reply-acks broken. The user then dropped `MEM_REMOTE` (`process_vm_*` are ptrace-class
calls an upstream reviewer would balk at) and two concerns: a VM process truncating a memfd the daemon
maps is accepted (a `SIGBUS` handler around the daemon's copies is possible later, premature now), and
a freshly booted VM losing its huge pages is accepted, as one memory path for every VM is wanted anyway
(board 14415).

**Guest RAM.** Every VM's RAM is a memory cell's window (board 14415): the daemon's own pages, mapped
from its cache read-only, and private copies in a private store. Review 176 settled how the daemon
reaches it:

- **The private store is the daemon's making.** The daemon creates the memory cell's private-copy
  memfd, sealed against shrinking and growing, when the memory cell opens, and hands it to the VM
  process, which today makes it itself (`Syscalls::private_memory_fd`, `cladefs_core::dax::uffd`).
  It recognises that inode, and no other, in the memory table.
- **A private map says which pages are private**, one bit per 4 KiB unit, which the VM process writes
  and the daemon maps read-only: today private state lives in the VM process alone
  (`PageChanges::private_mask`), per 4 KiB sub-page on 16 KiB hosts. The daemon reads a page from the
  private store if its bit is set, from its own cache otherwise. Cached state is forgotten at every
  snapshot, reclaim and reset, which end private pages.
- **Buffers by `pread`/`pwrite`, vrings mapped.** The daemon moves request data with `pread`/`pwrite`
  on the private store, so it holds no page-table entry on private pages that every reclaim and
  snapshot punch would have to flush from its CPUs (TASK-276.6); it maps only the vrings' pages.
- **Writing a page that isn't private** takes `MAKE_PRIVATE` first, batched: the VM process makes a
  vring's pages private at `SET_VRING_ADDR` and again before the queues restart after a snapshot;
  a request's other targets (a READ's data, the response) go in one `MAKE_PRIVATE` of several ranges,
  with `NO_COPY` where the daemon overwrites them whole. Step 0 measures a cold READ into
  never-written pages, which pays this.
- **The device and the memory cell are paired by the daemon**: they are two connections of one VM
  process (`MemoryCell::open` connects on its own), which the daemon pairs by the VM process (its
  `Peer`) and tenant. The memory table's region belongs to that memory cell, whichever of the VM's
  devices it reaches.
- **Plain memfd RAM too.** The backend also serves a stock front-end's guest RAM, a plain memfd, every
  page private, with no private map: what an upstream reviewer can run, and step 3's first test bed.
  Its writes to guest RAM are logged as vhost-user logs any (`VHOST_F_LOG_ALL`), for front-ends' dirty
  tracking.

**The window.** `SHMEM_LAZY` as doc-32 proposes it (`docs/design/doc-32 -
vhost-user-lazy-shared-memory-regions.md`), the protocol additions in full: one 64-bit table word per
unit of a stated size; write the entry, invalidate, reuse after the ack; installs under the lock
invalidations take; no message blocks on a reply; the feature bits it needs (`SHMEM`,
`BACKEND_REQ`, `BACKEND_SEND_FD`). Guest RAM as a lazy region stays a fork patch, as doc-32 leaves it
out of scope.

**Snapshots** stop the queues without a deadlock: the VM process serves the back-end channel on a
thread other than the one waiting for `GET_VRING_BASE`'s reply, as doc-32's section 7 requires.

Steps, each a board task under 14378:

0. **The baseline** (board 14378's A/B, before anything): today's path, on one test host, kept for every
   later step to be held against: fio and pgbench in a DAX guest (throughput and latency, the guest
   driver's `request_ns` and the daemon's serving time), a fresh boot's and a restore's time, a guest's
   first touch of a working set, and a cold READ into never-written pages.
1. **The interpreter** (board 14381): doc-4's protocol, `Buffers`, `handle`, generic over a `Dialect`,
   in `cladefs_core`.
2. **One memory path** (board 14415): a freshly booted VM's RAM a memory cell's window, by the code a
   restored VM's is; measured against step 0.
3. **The vhost-user backend for device type 0x4658** in cladefsd: its channel a socketpair the daemon
   hands the VM process over its existing connection once the cell is open, so it is bound to an
   authorized cell, and the VM process named by that connection (review 175, finding 1); its decoder
   our own, on checked cursors; protocol features `MQ`, `REPLY_ACK`, `CONFIG`, `BACKEND_REQ`,
   `DEVICE_STATE`, `RESET_DEVICE`, `SHMEM_LAZY`; each request served with the interpreter in the
   guest's dialect, the cell its backend (the parked `server/ring.rs`), its vrings walked checked;
   guest memory read and written through the private-copy memfd and the daemon's own pages, never
   holding a cell or window lock across a wait for the VM process; kick and call eventfds checked to be
   eventfds, kicks read with `preadv2(RWF_NOWAIT)` at offset -1 (Linux 5.8), calls written on the
   connection's thread. Snapshots stop every queue (`GET_VRING_BASE`) before the window is snapshotted.
4. **`SHMEM_LAZY` in the frontends**: the window stage driven by the table, in libmachine (Linux) first,
   one stage per VM process for guest RAM and the DAX window, sharing its mapping budget and vCPU hooks;
   the daemon's `DaxWindow` writing the table and invalidating in place of its stage operations.
5. **libmachine's frontend on Linux**, a generic vhost-user device, measured against step 0. macOS keeps
   its in-process daemon, `virtio_extent` and the socket's data requests.
6. **Firecracker**: series that each stand alone upstream: a generic vhost-user device; shared memory
   regions with `SHMEM_LAZY` (doc-32); vhost-user snapshots; reset. Each proposed there with its spec
   changes, the backend runnable against a stock Firecracker with plain memfd RAM; together they
   replace the fork's extent device. Measured against step 0.
7. **Local processes as frontends**, later, on Linux; the socket's data requests stay for macOS.
8. **The fork's extent device goes** on Linux, once both VMMs' frontends are generic.

[thp]: https://docs.kernel.org/admin-guide/mm/transhuge.html
[substrate]: https://github.com/agent-substrate/substrate/pull/1850
[hvi]: https://github.com/brig-sh/hvi-vmm/issues/401
[ch]: https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/memory.md
[fc6233]: https://github.com/firecracker-microvm/firecracker/pull/6233
[minor]: https://lore.kernel.org/all/20210427204250.GG6820@xz-x1/T/
[kalyazin]: https://lkml.org/lkml/2025/11/17/1532
