# Guest memory from cladefsd through lazy regions

Plan for board 14602's follow-up. The first version (2026-11-27) answered hotbed review 194 (Fable
5.2). This version (2026-11-28) follows the user's decision of 2026-11-28 in the extent-device
thread (board 14378), below. It supersedes the recommendation (option C) in
`guest-ram-external-uffd-handler.md`, which stays for its analysis of upstream's `Uffd` backend and
E2B. The question: how a VM's memory lives in cladefs cells, with Firecracker's side generic and
cladefs code out of our fork. **Derived** marks what follows from the code but was not run. Kernel
facts are from Linux v7.1.2 (`~/src/linux-7.1.2`), Firecracker's from v1.18.0 (`eaf83bb7`).

## The decision this follows (2026-11-28)

1. **No vhost-user extent device.** A back-end could reach a VM's private pages only through
   `process_vm_readv`, which the user judged too slow and too complex for I/O. The vhost-user
   crate and cladefsd's back-end leave main (boards 14537, 14603, 14620).
2. **doc-4's requests go over stock vsock**, Firecracker copying from guest memory. No Firecracker
   change; a prototype measured against step 0 comes first. That is the extent-device thread's
   work, not this plan's.
3. **The DAX window becomes a stock virtio-pmem device** whose memory is a lazy region like guest
   RAM. The cladefs guest driver binds the device in place of `virtio_pmem`.
4. **Window private pages** (file data the guest stored and hasn't written back) go into the
   memory cell's version as RAM's private pages do. A restore asks cladefsd for every region's list
   again, as a boot does.
5. **Upstream** in three steps on Firecracker's `Uffd` memory backend: (a) fresh boots, (b) pmem
   regions as well as RAM, (c) the handler sends runs to map. An upstream issue comes first, and
   only with the user's OK.

The fork is to carry only this lazy-region patch.

## The plan in one paragraph

A VM has **lazy regions**: its RAM, and each virtio-pmem device configured without a file (the DAX
window). cladefsd tells Firecracker which `mmap` calls make each region: a list of entries, each a
file range mapped `MAP_PRIVATE`, anonymous memory, or nothing (`PROT_NONE`). The files are
cladefsd's arena memfds, so every VM mapping an extent or a version's page shares the daemon's
page. A guest's store makes the host kernel copy that one page for that VM: a private page,
anonymous memory of the VM process. Firecracker registers the file entries with userfaultfd for
`MISSING` and `MINOR` faults and hands the userfaultfd to cladefsd, so a VM's first touch of any
page reaches cladefsd, which fills it if needed and installs it with `UFFDIO_CONTINUE`. cladefsd
sends a region a new list whenever it changes: a window's whenever the guest maps or unmaps part of
it (doc-4's `MAP` and `UNMAP`, now over vsock to cladefsd), while the guest runs. Making a version,
Firecracker first saves device state, then asks cladefsd to persist memory. cladefsd finds every
region's private pages with `PAGEMAP_SCAN`, reads them out of Firecracker with `process_vm_readv`,
writes them into the memory cell, and replies with lists that map the written ranges to the new
version's pages. A fresh boot is RAM as one anonymous entry and a window with nothing mapped.
Firecracker learns three messages (start, map, persist), all generic.

## Firecracker

A memory backend, `backend_type: "Mappings"` with a `backend_path` socket, offered at boot (`PUT
/memory-backend` and `memory-backend` in a configuration file, board 14609's field) and at load
(`mem_backend`). Its regions:

- **RAM:** reserved as today: anonymous, `MAP_NORESERVE`, DRAM then the virtio-mem region, laid out
  contiguously as a memory file is.
- **A pmem region:** a virtio-pmem device configured with `size_mib` and no `path_on_host`, whose
  memory the backend provides. Firecracker places it as it places any pmem device (2 MiB aligned,
  past the 64-bit MMIO gap, a KVM memory slot of its own) and reserves its host memory the same
  way. Its guest range and size are in the device's configuration space, as for any pmem device.

**Entries.** Each entry covers whole host pages of one region:
- **File:** a descriptor, file offset, region offset and length, mapped `MAP_PRIVATE`.
- **Zeros:** anonymous memory.
- **None:** `PROT_NONE`. A window's unmapped ranges, so a stray access fails instead of reading
  anything.

Firecracker applies each entry in three steps: `mmap` with `MAP_FIXED` and `PROT_NONE`, register
file entries with userfaultfd for `MISSING | MINOR`, then `mprotect` to read-write. So no access
reaches a file entry before it's registered, not even a stale one of KVM's async page fault worker.
An access in between fails, and KVM drops a failed async fault. Registered too late, a read of an
arena hole would put a zero page into the arena (`SGP_CACHE`) and into this VM's page table, where
cladefsd would never see it (review 194). **Derived:** registration checks VMA types and
`VM_MAYWRITE`, which a private mapping always has, never protection (`userfaultfd_register`), and
`mprotect` keeps a VMA's userfaultfd context.

The messages run on the VMM thread, which also runs device emulation, so no device touches guest
memory while one is in progress.

1. **Start**, at boot and at load, before the vCPUs run.
   - **Connect.** Firecracker connects to `backend_path` and sends the regions as upstream's `Uffd`
     handshake does (`GuestRegionUffdMapping` JSON), each named: RAM, or the pmem device's ID.
   - **Map.** The handler replies with a list per region and its descriptors (`SCM_RIGHTS`).
     Firecracker applies them, then applies virtio-mem's `PROT_NONE` to unplugged slots again
     (`MAP_FIXED` resets it).
   - **Hand over faults.** It creates a userfaultfd through `/dev/userfaultfd` (kernel faults
     included, as an unprivileged process can't otherwise; cladefs's TASK-308) and sends it to the
     handler. It requests no events: not `EVENT_REMOVE`, which would block the VMM thread in
     `madvise` until cladefsd read it. Registration happens per entry, as above.
   - **Fresh boot:** RAM is one zeros entry, each window one none entry.
2. **Map**, sent by the handler at any time: a list for one region, applied, then answered.
   - **While the guest runs**, for a window only. `MAP_FIXED` drops the private pages under the new
     entries, which is doc-4's `MAP` and `UNMAP`: either frees the private pages it replaces. The
     guest touches a window range only once its `MAP` has completed and stops before mapping over
     it. A vCPU's fault meanwhile waits on the VMM thread's `mmap_lock`.
   - **RAM** is remapped only by a persist, with the vCPUs paused. The guest may store to any RAM
     page at any time, and a remap would drop that store.
3. **Persist**, on `snapshot/create`:
   - **Device state first.** Firecracker pauses the vCPUs and saves device state (`save_state`:
     devices quiesce and write what they owe guest memory, the block queue drained, a deferred net
     frame delivered, arm64's GIC pending tables).
   - **Then memory.** It sends the handler a persist request and waits. The handler sends map
     messages for any region it remaps, then replies with an opaque JSON result.
   - **Answer.** Firecracker writes the state file and returns the handler's result as the API
     response. The state holds each region's guest range, never its contents or entries: a load
     asks the handler again (Start). No memory file: v1.18 requires `mem_file_path` (a `PathBuf`),
     which this backend makes optional. E2B's fork does the same.
4. **The handler going away** ends the VM: Firecracker exits with an error once the backend socket
   hangs up, as no one serves its faults any more. Today's fork stops the VM the same way when the
   daemon goes (`on_daemon_lost`).
5. **Nothing else.**
   - **Balloon and free page reporting** need no change. RAM's own mapping is the anonymous
     reservation, so `discard_range` takes its `madvise(MADV_DONTNEED)` branch (verified, review
     194). Over a file entry that drops the VM's private copies and shows the arena's page again,
     which is cladefs's reclaim. Over a zeros entry it gives zeros.
   - **pmem flushes** stay as upstream has them. The cladefs driver sends none, and an `msync` of a
     private mapping writes nothing.
   - **Huge pages** are refused with this backend.
   - **Seccomp:** the VMM's filter gains what the messages need: `recvmsg` with descriptors, `mmap`
     with `MAP_FIXED` of a descriptor, `mprotect`, the userfaultfd ioctls, and opening
     `/dev/userfaultfd`.

A list that fails part-way (`ENOMEM` at `vm.max_map_count`) is fatal to the VM. cladefsd keeps
every list within the VM's mapping budget, so this is a bug if it happens. Nothing in the backend
names cladefs. Upstream's `File` backend is the special case of one file entry covering RAM.

### What leaves the fork

All of the fork's guest-memory and window code:
- `cladefs_memory.rs`, `vmm_config/memory_cell.rs`, `PUT /memory-cell` and the `memory-cell` key.
- `MemBackendType::Cell` and `MemoryCellConfig` (with `diff_files`).
- `allocate_cell_memory`, `builder.rs`'s cladefs `allocate_guest_memory`,
  `StartMicrovmError::CellMemory`, `GuestRegionMmap::from_unowned`.
- The reclaim hook in `discard_range`, and the `VhostUser` refusal.
- `snapshot/create`'s `memory_cell` (`CreateMemoryCellConfig`, `MemoryCellSnapshot`).
- The extent device's window (`devices/virtio/extent/window.rs` with `KvmPrefault`), and the PCI
  transport's shared memory BARs (`cd9ec3950`), which only the window uses.
- The vCPU hooks: `guest_thread_started`, `AccessFailures` and the `EFAULT` retries in `vcpu.rs`.
  They exist for today's second stage, which write-protects pages and changes them in several
  calls. Here a store copies in the kernel, and an entry is inaccessible until registered.

The rest of the extent device (its requests, notices and snapshot state) leaves with the move to
vsock (point 2). The fork's other commits (`fix(vsock)`, `fix(jailer)`, `fix(signal)`,
`fix(virtio)`, `fix(vcpu)`) aren't cladefs code: each goes upstream or is dropped.

### Frozen formats

Two changes need the user's explicit approval before any step that makes them:

- **`snapshot-extent-state`** is Firecracker's state of an extent device: its window's place and
  every mapping (`MappingStateV2`). A Firecracker without the device can't load a state file that
  has one. This comes with point 2 as well as this plan.
- **The window's mappings move into the memory cell.** A version must record which extent range
  each window range maps, which today's state file does. With point 4 the state file holds only
  guest ranges, so cladefsd stores the mappings: a new record in the memory cell, a new format
  family.

## cladefsd

1. **Binding.** The control plane registers a VM with cladefsd before starting it: its memory cell
   (a new one for a boot, a fork of a version or an ephemeral cell for a restore), the disk cell its
   window maps, its tenant, and a socket made for that VM alone, which the jail exposes as
   `backend_path`. One connection per socket. At connect cladefsd takes a pidfd of the peer
   (`SO_PEERPIDFD`). It checks the pidfd is still live after opening `/proc/<pid>/pagemap` and
   after each `process_vm_readv`, so a recycled PID never reads another VM. The userfaultfd it
   receives is checked to be one (`[userfaultfd]` anonymous inode). Its mm can't be checked, but
   one Firecracker could hand over would be its own or a child's, and serving it only fills memory
   that process may read anyway.
2. **Per-VM threads.** A VM's faults, map messages and persist run on threads of that VM's own,
   never shared, because each can block on that VM's memory: a fault, or a page the VM process
   remapped with a userfaultfd of its own. A persist past its deadline kills the VM through its
   pidfd, which ends any copy blocked on it.
3. **The lists.**
   - **RAM**, from the memory cell's image: a run of data maps its part at that part's arena
     offset, and a hole is a zeros entry.
   - **A window** at a restore: the memory cell's record of its mappings, each range mapping an
     extent of the disk cell's or zeros, with the private pages the version stored over them. Those
     map the memory cell's own parts. The rest is none. At a boot it's all none.
   - **A guest's `MAP` and `UNMAP`** arrive over vsock. cladefsd checks them as doc-4 says, sends
     Firecracker the window's new entries, and answers the guest once Firecracker has applied them.
     `MAP_ZEROS` is a zeros entry, `UNMAP` a none entry.
   - **Descriptors** go read-only: a writable private mapping needs only read access, the VM
     process already holds the default arena read-only (`Memories::fd`'s `O_RDONLY` check in
     `cladefs_core::dax::uffd`), and a read-only descriptor can't be reopened writable
     (`slabs/object.rs`). Reading its tenant's arena and the default arena is doc-26's accepted
     exposure.
   - **16 KiB hosts** (arm64): entries are whole host pages. Where runs don't align to them,
     cladefsd composes the host page in the arena, as today's daemon does.
4. **Fixed arena offsets.** Firecracker maps arena offsets and keeps them, so a part named in a
   live list, a memory cell's or an extent's, keeps its offset until no list names it (the VM
   exited, a remap replaced it, or the guest mapped over it). Eviction punches a hole there. The
   kernel drops the page from every mapping and leaves private pages alone (`shmem_fallocate`:
   "hole-punching leaves COWed pages"). The next touch faults `MISSING`, and cladefsd fills the
   same offset again. This replaces the slots doc-26 reuses after its quarantine, for every part a
   list names. `specs/part-slabs` changes with it. The arena is in-memory, in no `formats/` family.
5. **Faults: everything goes through cladefsd.** With `MINOR` registered, a VM's first touch of a
   page already in the arena faults to cladefsd too (`shmem_get_folio_gfp` raises `MINOR` for any
   folio in the page cache, whoever made it). cladefsd installs it with `UFFDIO_CONTINUE`, which on
   a private mapping installs the page read-only (`mfill_atomic_install_pte`: `page_in_cache &&
   !vm_shared`), so a store copies it in the kernel. Three reasons, all from review 194:
   - **Holes can't be poisoned.** A read of an arena hole through any mapping not registered with
     userfaultfd inserts a zero page into the arena (`SGP_CACHE`). An escaped VM process could do
     that on the default arena, which every VM process holds, and other VMs would then read zeros
     with no fault. Under `MINOR`, a VM's touch of such a page still reaches cladefsd. cladefsd
     knows which offsets it filled, and it fills any other page with its real contents (`pwrite`
     over the zeros) before installing it.
   - **Fills can't tear.** A page cladefsd is filling is in the arena before its bytes are. A VM
     touching it reaches cladefsd, which installs it only once filled.
   - **Loads stay shared.** cladefsd installs a whole node (RAM) or the mapped range around the
     fault (a window) with `UFFDIO_CONTINUE` on the first fault in it, and RAM's nodes ahead at
     restore. A load of an installed page needs no fault. A first touch through KVM's async page
     fault worker copies only the page it faulted on, as today
     (`kvm-async-pf-and-shared-guest-memory.md` §4). It wakes the fault on every message.
   - **Filling** writes with `pwrite`, page-atomic under the folio lock, or through a mapping of its
     own made for the fill and removed before anything there is punched. doc-26 keeps the daemon's
     page tables off the arena so that punches flush no TLB of its (TASK-276.6).
6. **Persist** (Firecracker waiting, devices quiesced):
   1. **Find the private pages.** `PAGEMAP_SCAN` on the VM process's pagemap over each region, for
      anonymous pages: present without `PAGE_IS_FILE` and not `PAGE_IS_PFNZERO`, and swapped
      without `PAGE_IS_FILE`. A zeros entry's pages count only where present: the guest wrote them.
      Under the lists those are exactly the pages the VM wrote, its devices' writes included. THP,
      the zero page, swap, migration entries and KSM classify correctly (review 194,
      `pagemap_page_category`). No dirty logging, so no `track_dirty_pages`.
   2. **Read them.** `process_vm_readv` of those ranges into arena offsets of the extents they
      become, through a mapping made for the copy (point 5). One copy, no intermediate file. Pages
      of zeros become holes (`runs_of_zeros`), as `write_sparse` does now.
   3. **Store them.** RAM's at their guest offsets in the memory cell's image, a window's in a part
      of the image of its own, and the window's mappings in their record. Commit, then make the
      version or checkpoint the ephemeral cell. The new parts can't be evicted until the commit.
   4. **Remap.** Map messages for each region: written ranges map the new parts, and RAM's ranges
      of zeros become zeros entries. This ends the private pages and shares them again, so the next
      version reads only what the guest writes after this one. Then the reply.
7. **The mapping budget.** Each file entry is a VMA, and remaps split entries. cladefsd keeps a
   VM's entries within a share of `vm.max_map_count`, as the second stage's budget does today
   (`budget`, `cladefs_core::dax::uffd`).
   - **RAM:** when a remap would pass the share, cladefsd rewrites the fragmented nodes whole into
     the new version, unchanged pages too, so each maps as one entry.
   - **A window:** a guest `MAP` that would pass the share fails with `ENOMEM`. The guest driver
     unmaps ranges for room, as it does when the window is full (`window_full`, `reclaims`), and
     maps again. cladefsd never remaps a window on its own while the guest runs, since a remap
     would drop the guest's stores there.
   - **Hosts** raise `vm.max_map_count` (E2B sets 1,048,576).
8. **The governor** counts the arena as before. Two things are new: fixed offsets leave slabs
   partly filled, and a persist allocates the private pages' new parts inside the pause. The budget
   must hold room for the latter, as it does for today's `snapshot_window`. A window's private
   pages are the VM process's memory, which its cgroup limits, as RAM's are. cladefsd no longer
   reserves memory for them.
9. **Access and host settings.**
   - `process_vm_readv` needs `PTRACE_MODE_ATTACH` (`mm/process_vm_access.c`). cladefsd runs as
     root, and Yama's `ptrace_scope` must not be 3.
   - Linux 6.7 or later (`PAGEMAP_SCAN`, and `SO_PEERPIDFD` from 6.5).
   - shmem large folios off for the arena: a large folio would fill and fault a whole range at
     once, defeating per-page `MINOR` handling.
   - What it reads is the guest's data, stored as is: no trust is placed in it.

## Guest kernel

- **The cladefs driver binds virtio-pmem** (device ID 27) in place of `virtio_pmem`, and takes its
  window's guest range and size from the device's configuration space. Its requests go over vsock
  (point 2, the extent-device thread's work).
- **doc-4 changes:**
  - `MAP` can fail with `ENOMEM` when the host's mapping budget is spent (cladefsd, point 7).
  - `PRIVATE` and `MAP_PREFAULT` lose their host side. `KVM_PRE_FAULT_MEMORY` was fork code, and a
    store to a page not private yet now copies it in KVM's own fault, with no exit to userspace.
    Both become hints cladefsd may ignore; measured before either is removed.
  - `private_max` reserves nothing in the daemon any more. The guest keeps under it, and the VM
    process's cgroup limits it.
  - A version no longer needs the window's private pages rebound: the host stops refusing a point
    for them (doc-4, Snapshots). Freezing stays for its other purposes.

## What it costs

- **Window maps take two more hops.** Today a guest's `MAP` reaches the window's stage inside the
  VM process. Here it goes over vsock to cladefsd, which sends Firecracker a map message, waits for
  it, and answers over vsock, each entry three system calls. `MAP_MANY` batches them. Measured with
  the vsock prototype against step 0.
- **Faults per page.** Every VM's first touch of a node or mapped range reaches cladefsd, which
  installs it with `UFFDIO_CONTINUE`: an ioctl per node per VM, no mapping each. Today's second
  stage installs a node's pages ahead the same way.
- **Pausing** for a version covers device state, the scan of every region, the read, the commit
  and the remap. Today's `snapshot_window` writes, commits and reclaims inside the pause too.
- **macOS.** libmachine-firecracker has no userfaultfd. It can take the same lists and fill
  eagerly, as `MemoryCell::window_in_ram` maps the whole image now. Later.

## Upstreaming

Each step builds on Firecracker's `Uffd` memory backend and is a patch of its own in the series,
named for no one. An upstream issue proposing them comes first, filed only with the user's OK.

- **(a) Fresh boots** (board 14609): a `Uffd` backend at boot, guest memory made, registered and
  handed over by the code `guest_memory_from_uffd` runs on load. The handshake is unchanged.
- **(b) pmem regions** (board 14665): a virtio-pmem device with `size_mib` and no `path_on_host`,
  whose memory the backend serves as it serves RAM. The handshake lists it as a region. Its state
  holds its guest range, and a load hands it to the handler again.
- **(c) The handler sends runs to map** (board 14628): the handshake reply's lists, the map
  message at any time, entries registered `MISSING | MINOR` through `/dev/userfaultfd`, the persist
  message in place of a memory file, and exiting when the handler goes. This is what `Mappings`
  names above: a `Uffd` backend whose handler answers the handshake with lists.

## Steps

1. **(a) Firecracker: fresh boots with the `Uffd` backend** (board 14609).
2. **(b) Firecracker: pmem regions served by the backend** (board 14665).
3. **(c) Firecracker: the handler sends runs to map** (board 14628): entries applied unregistered
   only while inaccessible, map messages while the guest runs (pmem regions) and at persist,
   persist after device state, exit when the handler goes, seccomp.
4. **cladefsd: fixed arena offsets for every part a list names** (board 14629): eviction by
   punching, offsets released when no live list names them, `specs/part-slabs` and the governor
   updated.
5. **cladefsd: the backend's handler** (board 14630): binding by the control plane, pidfds, per-VM
   threads, RAM and window lists, guest `MAP` and `UNMAP` as map messages, `MISSING` and `MINOR`
   faults filled and installed, persist of every region with the remap, the mapping budget. The
   window's mappings record waits for the user's approval (Frozen formats).
6. **Guest: the cladefs driver binds the window's virtio-pmem device** (board 14666), and retries a
   `MAP` that fails with `ENOMEM` after reclaiming window ranges. Its requests come over vsock once
   the extent-device thread's prototype lands.
7. **Remove the fork's memory and window code** (board 14631), guest tests through the backend,
   measured against plan v3's step 0 baseline (a fresh boot, a restore, the bench's first touch of
   1 GiB, `b73e6d1ab`).
