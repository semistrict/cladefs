# Guest RAM from a memory cell through a mapping list

Plan for board 14602's follow-up (2026-11-27). This supersedes the recommendation (option C) in
`guest-ram-external-uffd-handler.md`, which stays for its analysis of upstream's `Uffd` backend and
E2B. The question is unchanged: how a VM's RAM image lives in a memory cell, with Firecracker's side
generic and the cladefs code out of our fork. **Derived** marks what follows from the code but was
not run. Kernel facts are from Linux v7.1.2 (`~/src/linux-7.1.2`), Firecracker's from v1.18.0
(`eaf83bb7`).

## The plan in one paragraph

cladefsd tells Firecracker which `mmap` calls make guest RAM: a list of `{file, file offset, guest
memory offset, length}`, each mapped `MAP_PRIVATE`. The files are cladefsd's arena memfds, so every VM
restored from a version shares the daemon's pages, and a guest's store makes the host kernel copy
that one page for that VM: a private page, anonymous memory of the VM process. Pages not loaded
yet fault to cladefsd through userfaultfd, as upstream's `Uffd` backend hands faults over. A fresh
boot is the same with an empty list. Making a version, with the VM paused, cladefsd finds the
private pages with `PAGEMAP_SCAN`, reads them out of Firecracker with `process_vm_readv`, writes
them into the memory cell, and then sends a second list that maps the written ranges to the new
version's pages, which ends the private pages. Firecracker learns two messages, both generic.

## Firecracker

A memory backend, `backend_type: "Mappings"` with a `backend_path` socket, offered at boot (`PUT
/memory-backend` and `memory-backend` in a configuration file, board 14609's field) and at load
(`mem_backend`):

1. **Start.** Firecracker reserves guest memory as today: anonymous, `MAP_NORESERVE`, DRAM then
   the virtio-mem region, laid out contiguously as a memory file is. It connects to `backend_path`
   and sends the regions as upstream's `Uffd` handshake does (`GuestRegionUffdMapping` JSON). The
   handler replies with the mapping list and its file descriptors (`SCM_RIGHTS`). Firecracker maps
   each entry `MAP_PRIVATE | MAP_FIXED` over the reservation. It creates a userfaultfd
   (`EVENT_REMOVE`, as upstream asks), registers `MISSING` over the mapped entries, and sends it to
   the handler. Gaps between entries stay anonymous memory: holes, zeros, no faults. An empty list
   is a fresh boot.
2. **Remap, while paused.** The handler sends another list over the same socket. Firecracker refuses
   it unless the VM is paused, maps each entry over what is there, registers the new mappings, and
   replies. With the vCPUs stopped, no guest access can land between a mapping's replacement and
   its registration. That window is what makes today's fork retry `EFAULT` in the vCPU loop.
3. **Device state without memory.** `snapshot/create` saves the VM's state without writing guest
   memory, when a `Mappings` backend holds it. v1.18 requires `mem_file_path` (a `PathBuf`), so this
   is a small change. E2B's fork carries the same.
4. **Nothing else.** Balloon discard and free page reporting need no change (**derived**). The
   region's own mapping is the anonymous reservation, so `discard_range` takes its `madvise(MADV_DONTNEED)`
   branch. Over an entry that drops the VM's private copies and shows the arena's page again,
   which is cladefs's reclaim. Over a gap it gives zeros. The handler sees `UFFD_EVENT_REMOVE` and
   needs to do nothing. Huge pages are refused with this backend: entries are 4 KiB pages. vhost-user
   devices are refused, as on restored VMs upstream: a back-end can't reach private pages (below).
   The VMM's seccomp filter gains what the two messages need (`recvmsg` with descriptors, `mmap`
   with `MAP_FIXED` of a descriptor, the userfaultfd ioctls).

Nothing in it names cladefs. Upstream's `File` backend is the special case of one entry covering
everything.

**What leaves the fork.** `cladefs_memory.rs`, `vmm_config/memory_cell.rs`, `PUT /memory-cell` and the
`memory-cell` key, `MemBackendType::Cell` and `MemoryCellConfig` (with `diff_files`),
`allocate_cell_memory`, `builder.rs`'s cladefs `allocate_guest_memory`, `StartMicrovmError::CellMemory`,
`GuestRegionMmap::from_unowned`, the reclaim hook in `discard_range`, the `VhostUser` refusal, and
`snapshot/create`'s `memory_cell` (`CreateMemoryCellConfig`, `MemoryCellSnapshot`). The vCPU hooks
(`guest_thread_started`, `AccessFailures`) stay while the extent device's DAX window needs them.

## cladefsd

1. **The backend's handler.** On a connection it opens the VM's memory cell: a new, empty one for a
   boot (`MemoryCell::create`, `create_ephemeral`); a fork of the version, or an ephemeral cell
   reopened, for a restore. It turns the image's runs into the mapping list. A run of data maps its
   extent's part at that part's arena offset. A hole maps nothing. It sends the arena descriptors
   read-only: a writable private mapping needs only read access to its file, and the VM process
   already holds the default arena read-only (`Memories::fd`'s `O_RDONLY` check in
   `cladefs_core::dax::uffd`).
2. **Fixed arena offsets.** Firecracker maps arena offsets once and keeps them. So a part named in a
   live mapping list keeps its offset until no list names it. Eviction punches a hole there. The
   kernel then drops the page from every mapping, leaving private pages alone (`shmem_fallocate`:
   "hole-punching leaves COWed pages"). The next touch faults `MISSING` to cladefsd, which fills the
   same offset again. This replaces the slots doc-26 reuses after its quarantine, for memory cells'
   parts: an offset is reused only once every list naming it is gone (the VM exited, or a remap
   replaced it). The arena is in-memory, so no frozen format changes.
3. **Faults.** A `MISSING` fault names a page of an arena hole. cladefsd fills it, and the rest of
   its node, into the arena through its own mapping, then wakes the fault (`UFFDIO_WAKE`). Never
   `UFFDIO_COPY`: on a private mapping that makes an anonymous copy, not a shared page. Filling a
   node at a time matters for KVM's async page faults. A load of a page already in the arena
   resolves in the kernel and stays shared. A load of one not filled goes through the async worker,
   whose write copies that page. So at most a node's first page becomes private for a load, as
   today (`kvm-async-pf-and-shared-guest-memory.md` §4).
4. **Making a version** (the control plane pauses the VM first):
   1. `PAGEMAP_SCAN` on `/proc/<pid>/pagemap` over guest memory, asking for anonymous pages: present
      pages without `PAGE_IS_FILE` and not `PAGE_IS_PFNZERO`, and swapped ones without
      `PAGE_IS_FILE`. Under the mapping list those are exactly the pages the VM wrote, Firecracker's
      device writes included (`pagemap_page_category`, `fs/proc/task_mmu.c`). No dirty logging, so
      no `track_dirty_pages`.
   2. `process_vm_readv` of those ranges straight into the arena, at offsets of the extents they
      will be written as. Pages of zeros become holes (`runs_of_zeros`), as `write_sparse` does
      now. Then cladefsd writes them into the memory cell, commits, and makes the version, or
      checkpoints an ephemeral cell. One copy, as today's fork makes, and no intermediate file.
   3. The remap message: the written ranges now map the new extents' arena offsets, which ends
      the private pages and shares the pages again. The next version reads only what the guest
      writes after this one. Without it, every version would read every page written since the
      restore.
   4. Firecracker saves its device state (point 3 above), and the VM resumes.
5. **Access.** `PAGEMAP_SCAN` and `process_vm_readv` need ptrace-read access to the VM process:
   cladefsd runs as root, and Yama's `ptrace_scope` must not be 3. Linux 6.7 or later
   (`PAGEMAP_SCAN`). What it reads is the guest's data, stored as is: no trust is placed in it.

## Guest kernel

No change.

## What it costs and what is open

- **Mappings.** Each entry is a VMA. Adjacent runs of one extent, contiguous in the arena, merge
  into one. A badly scattered image could take one per node, about 1,500 per GiB, and remaps add
  more. Hosts raise `vm.max_map_count` (E2B sets 1,048,576). To measure.
- **vhost-user.** A back-end can't reach private pages, which are in no file. Today's fork refuses
  vhost-user with a memory cell too (`CellMemoryError::VhostUser`), so this is no regression. Plan
  v3 (board 14378) has to settle how its back-end reaches guest RAM, separately.
- **Pausing to make a version** takes a pagemap scan, the read, the commit and a remap. Today's
  `snapshot_window` writes, commits and reclaims inside the pause as well.
- **macOS.** libmachine-firecracker has no userfaultfd. It can take the same list and fill
  eagerly, as `MemoryCell::window_in_ram` maps the whole image now. Later.
- **Async page faults.** First touches of unfilled nodes copy one page each, as today. Filling
  nodes ahead at restore shrinks it.

## Steps

1. **Firecracker: the `Mappings` backend** at boot and load (start message, `MISSING`
   registration, the remap message while paused, device state without memory, seccomp). On top of
   board 14609's boot-time backend field.
2. **cladefsd: fixed arena offsets for memory cells' parts**, eviction by punching, offsets released
   when no live list names them.
3. **cladefsd: the backend's handler**: mapping lists from images, `MISSING` faults filled a node at
   a time, making versions by `PAGEMAP_SCAN` and `process_vm_readv`, the remap after.
4. **Remove the fork's memory-cell code**, guest tests through `Mappings`, measured against plan v3's
   step 0 baseline (a fresh boot, a restore, the bench's first touch of 1 GiB, `b73e6d1ab`).
