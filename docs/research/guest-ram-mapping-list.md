# Guest RAM from a memory cell through a mapping list

Plan for board 14602's follow-up (2026-11-27). Reviewed in hotbed review 194 (Fable 5.2); this
version answers its findings. It supersedes the recommendation (option C) in
`guest-ram-external-uffd-handler.md`, which stays for its analysis of upstream's `Uffd` backend and
E2B. The question is unchanged: how a VM's RAM image lives in a memory cell, with Firecracker's side
generic and the cladefs code out of our fork. **Derived** marks what follows from the code but was
not run. Kernel facts are from Linux v7.1.2 (`~/src/linux-7.1.2`), Firecracker's from v1.18.0
(`eaf83bb7`).

## The plan in one paragraph

cladefsd tells Firecracker which `mmap` calls make guest RAM: a list of entries, each either
`{file, file offset, guest memory offset, length}`, mapped `MAP_PRIVATE`, or anonymous memory. The
files are cladefsd's arena memfds, so every VM restored from a version shares the daemon's pages.
A guest's store makes the host kernel copy that one page for that VM: a private page, anonymous
memory of the VM process. Firecracker registers the file entries with userfaultfd for `MISSING`
and `MINOR` faults and hands the userfaultfd to cladefsd. So a VM's first touch of any page reaches
cladefsd, which fills it if needed and installs it with `UFFDIO_CONTINUE`, a node at a time and
ahead of the guest. A fresh boot is the same with a list of one anonymous entry. Making a version,
Firecracker first saves device state as it always does, then asks cladefsd to persist memory and
waits. cladefsd finds the private pages with `PAGEMAP_SCAN`, reads them out of Firecracker with
`process_vm_readv`, writes them into the memory cell, and replies with a second list that maps the
written ranges to the new version's pages. Firecracker applies it before it answers. Firecracker
learns three messages, all generic.

## Firecracker

A memory backend, `backend_type: "Mappings"` with a `backend_path` socket, offered at boot (`PUT
/memory-backend` and `memory-backend` in a configuration file, board 14609's field) and at load
(`mem_backend`). Every exchange runs on the VMM thread, which also runs device emulation, so no
device touches guest memory while one is in progress. The vCPUs are not running either: not
started yet, or paused.

1. **Start.**
   - **Reserve and connect.** Firecracker reserves guest memory as today: anonymous,
     `MAP_NORESERVE`, DRAM then the virtio-mem region, laid out contiguously as a memory file is.
     It connects to `backend_path` and sends the regions as upstream's `Uffd` handshake does
     (`GuestRegionUffdMapping` JSON).
   - **Map.** The handler replies with the list and its file descriptors (`SCM_RIGHTS`).
     Firecracker maps each file entry `MAP_PRIVATE | MAP_FIXED` over the reservation, and each
     anonymous entry as anonymous memory. It then applies virtio-mem's `PROT_NONE` to unplugged
     slots again (`MAP_FIXED` resets it).
   - **Hand over faults.** It creates a userfaultfd through `/dev/userfaultfd` (kernel faults
     included, as an unprivileged process can't otherwise; cladefs's TASK-308). It requests no
     events: not `EVENT_REMOVE`, which would block the VMM thread in `madvise` until cladefsd read
     it. It registers `MISSING | MINOR` over the file entries and sends the userfaultfd to the
     handler.
   - **Fresh boot:** the list is one anonymous entry.
2. **Persist.** `snapshot/create` with a `Mappings` backend:
   - **Device state first.** Firecracker pauses the vCPUs and saves device state (`save_state`:
     devices quiesce and write what they owe guest memory, the block queue drained, a deferred net
     frame delivered, arm64's GIC pending tables).
   - **Then memory.** It sends the handler a persist request and waits. The handler's reply
     carries a list to map, applied as at start (each entry mapped, then registered), and an opaque
     JSON result.
   - **Answer.** Firecracker writes the state file and returns the handler's result as the API
     response. No memory file: v1.18 requires `mem_file_path` (a `PathBuf`), which this backend
     makes optional. E2B's fork does the same.
3. **The handler going away** ends the VM: Firecracker exits with an error once the backend socket
   hangs up, as no one serves its faults any more. Today's fork stops the VM the same way when the
   daemon goes (`on_daemon_lost`).
4. **vhost-user** works with this backend (the vhost-user thread's decision, board 14378).
   Back-ends reach guest RAM with `process_vm_readv` and `process_vm_writev` at the VM process's
   addresses, so Firecracker sends them a memory table for RAM that isn't one plain memfd:
   addresses only. A back-end's write into guest RAM pins the page for writing (`FOLL_WRITE`,
   `mm/process_vm_access.c`), which copies it under `MAP_PRIVATE` as a guest store does. So the
   next persist captures it.
5. **Nothing else.**
   - **Balloon and free page reporting** need no change. The region's own mapping is the
     anonymous reservation, so `discard_range` takes its `madvise(MADV_DONTNEED)` branch
     (verified, review 194). Over a file entry that drops the VM's private copies and shows the
     arena's page again, which is cladefs's reclaim. Over an anonymous entry it gives zeros.
   - **Huge pages** are refused with this backend: file entries are 4 KiB pages.
   - **Seccomp:** the VMM's filter gains what the messages need (`recvmsg` with descriptors,
     `mmap` with `MAP_FIXED` of a descriptor, the userfaultfd ioctls, opening `/dev/userfaultfd`).

A list that fails part-way (`ENOMEM` at `vm.max_map_count`) is fatal to the VM. cladefsd keeps
every list within the VM's mapping budget, so this is a bug if it happens. Nothing in the backend
names cladefs. Upstream's `File` backend is the special case of one file entry covering everything.

**What leaves the fork:**
- `cladefs_memory.rs`, `vmm_config/memory_cell.rs`, `PUT /memory-cell` and the `memory-cell` key.
- `MemBackendType::Cell` and `MemoryCellConfig` (with `diff_files`).
- `allocate_cell_memory`, `builder.rs`'s cladefs `allocate_guest_memory`,
  `StartMicrovmError::CellMemory`, `GuestRegionMmap::from_unowned`.
- The reclaim hook in `discard_range`, and the `VhostUser` refusal.
- `snapshot/create`'s `memory_cell` (`CreateMemoryCellConfig`, `MemoryCellSnapshot`).

The vCPU hooks (`guest_thread_started`, `AccessFailures`) stay while the extent device's DAX window
needs them.

## cladefsd

1. **Binding.** The control plane registers a VM with cladefsd before starting it: its memory cell
   (a new one for a boot, a fork of a version or an ephemeral cell for a restore), its tenant, and
   a socket made for that VM alone, which the jail exposes as `backend_path`. One connection per
   socket. At connect cladefsd takes a pidfd of the peer (`SO_PEERPIDFD`). It checks the pidfd is
   still live after opening `/proc/<pid>/pagemap` and after each `process_vm_readv`, so a recycled
   PID never reads another VM. The userfaultfd it receives is checked to be one (`[userfaultfd]`
   anonymous inode). Its mm can't be checked, but one Firecracker could hand over would be its own
   or a child's, and serving it only fills memory that process may read anyway.
2. **Per-VM threads.** A VM's faults, its persist and its vhost-user copies run on threads of that
   VM's own, never shared, because each can block on that VM's memory: a fault, or a page the VM
   process remapped with a userfaultfd of its own. A persist past its deadline kills the VM through
   its pidfd, which ends any copy blocked on it.
3. **The list.** From the image's runs: a run of data maps its extent's part at that part's arena
   offset, and a hole is an anonymous entry. The arena descriptors go read-only: a writable private
   mapping needs only read access, the VM process already holds the default arena read-only
   (`Memories::fd`'s `O_RDONLY` check in `cladefs_core::dax::uffd`), and a read-only descriptor
   can't be reopened writable (`slabs/object.rs`). Reading its tenant's arena and the default
   arena is doc-26's accepted exposure.
4. **Fixed arena offsets.** Firecracker maps arena offsets and keeps them, so a part named in a
   live list keeps its offset until no list names it (the VM exited, or a persist's remap replaced
   it). Eviction punches a hole there. The kernel drops the page from every mapping and leaves
   private pages alone (`shmem_fallocate`: "hole-punching leaves COWed pages"). The next touch
   faults `MISSING`, and cladefsd fills the same offset again. This replaces the slots doc-26 reuses
   after its quarantine, for memory cells' parts. `specs/part-slabs` changes with it. The arena is
   in-memory, in no `formats/` family.
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
   - **Loads stay shared.** cladefsd installs a whole node with `UFFDIO_CONTINUE` on the first
     fault in it, and nodes ahead at restore. A load of an installed page needs no fault. A first
     touch through KVM's async page fault worker copies only the page it faulted on, as today
     (`kvm-async-pf-and-shared-guest-memory.md` §4). It wakes the fault on every message.
   - **Filling** writes with `pwrite`, page-atomic under the folio lock, or through a mapping of its
     own made for the fill and removed before anything there is punched. doc-26 keeps the daemon's
     page tables off the arena so that punches flush no TLB of its (TASK-276.6).
6. **Persist** (Firecracker waiting, devices quiesced):
   1. **Find the private pages.** `PAGEMAP_SCAN` on the VM process's pagemap over guest memory, for
      anonymous pages: present without `PAGE_IS_FILE` and not `PAGE_IS_PFNZERO`, and swapped
      without `PAGE_IS_FILE`. Under the list those are exactly the pages the VM wrote, its devices'
      and vhost-user back-ends' writes included. THP, the zero page, swap, migration entries and KSM
      classify correctly (review 194, `pagemap_page_category`). No dirty logging, so no
      `track_dirty_pages`.
   2. **Read them.** `process_vm_readv` of those ranges into arena offsets of the extents they become,
      through a mapping made for the copy (point 5). One copy, no intermediate file. Pages of zeros
      become holes (`runs_of_zeros`), as `write_sparse` does now.
   3. **Store them.** Write them into the memory cell, commit, make the version or checkpoint the
      ephemeral cell. The new parts can't be evicted until the commit.
   4. **Reply with the remap.** Written ranges map the new parts, and ranges of zeros become
      anonymous entries. This ends the private pages and shares them again, so the next version
      reads only what the guest writes after this one.
7. **The mapping budget.** Each file entry is a VMA, and a remap splits entries. cladefsd keeps a
   VM's entries within a share of `vm.max_map_count`, as the second stage's budget does today
   (`budget`, `cladefs_core::dax::uffd`). When a remap would pass it, cladefsd rewrites the
   fragmented nodes whole into the new version, unchanged pages too, so each maps as one entry.
   Hosts raise `vm.max_map_count` (E2B sets 1,048,576).
8. **The governor** counts the arena as before. Two things are new: fixed offsets leave slabs
   partly filled, and a persist allocates the private pages' new parts inside the pause. The budget
   must hold room for the latter, as it does for today's `snapshot_window`.
9. **Access and host settings.**
   - `process_vm_readv` needs `PTRACE_MODE_ATTACH` (`mm/process_vm_access.c`). cladefsd runs as
     root, and Yama's `ptrace_scope` must not be 3.
   - Linux 6.7 or later (`PAGEMAP_SCAN`, and `SO_PEERPIDFD` from 6.5).
   - shmem large folios off for the arena: a large folio would fill and fault a whole range at
     once, defeating per-page `MINOR` handling.
   - What it reads is the guest's data, stored as is: no trust is placed in it.

## Guest kernel

No change.

## What it costs and what is open

- **Faults per page.** Every VM's first touch of a node reaches cladefsd, which installs the node
  with `UFFDIO_CONTINUE`: an ioctl per node per VM, no mapping each. Today's second stage installs
  a node's pages ahead the same way.
- **Pausing** for a version covers device state, the scan, the read, the commit and the remap.
  Today's `snapshot_window` writes, commits and reclaims inside the pause too.
- **macOS.** libmachine-firecracker has no userfaultfd. It can take the same list and fill eagerly,
  as `MemoryCell::window_in_ram` maps the whole image now. Later.

## Steps

1. **Firecracker: the `Mappings` backend** at boot and load: start (list, `MISSING | MINOR`
   registration through `/dev/userfaultfd`, virtio-mem slots), persist on `snapshot/create` after
   device state, exit when the handler goes, the vhost-user memory table, seccomp. On top of board
   14609's boot-time backend field.
2. **cladefsd: fixed arena offsets for memory cells' parts**: eviction by punching, offsets
   released when no live list names them, `specs/part-slabs` and the governor updated.
3. **cladefsd: the backend's handler**: binding by the control plane, pidfds, per-VM threads, lists
   from images, `MISSING`/`MINOR` faults filled and installed a node at a time, persist by
   `PAGEMAP_SCAN` and `process_vm_readv` with the remap reply, the mapping budget.
4. **Remove the fork's memory-cell code**, guest tests through `Mappings`, measured against plan
   v3's step 0 baseline (a fresh boot, a restore, the bench's first touch of 1 GiB, `b73e6d1ab`).
