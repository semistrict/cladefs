# Guest RAM through an external userfaultfd handler, fresh boots too

Research for board 14602 (2026-11-27); decision recorded as hotbed question 622, reviewed in hotbed
review 193. The question: can upstream Firecracker's `Uffd` memory backend, with cladefsd (or a
helper) as its outside handler and extended so a fresh boot uses it as a restore does, replace the
fork's guest-RAM code (`cladefs_memory.rs`, the `memory-cell` boot configuration, `PUT
/memory-cell`, the `Cell` memory backend)? Board 14378's plan v3 keeps guest RAM as a fork patch
(doc-32 leaves it out of scope); this covers that gap.

Sources: upstream Firecracker v1.18.0 (`eaf83bb7`, the series base in `vendor/firecracker.base`); the
fork at `cladefs-v1.18.0-r8` (`cdd1d886d`); Linux v7.1.2 (`~/src/linux-7.1.2`); e2b-infra at
`75d4fae` (`~/src/e2b-infra`). **Derived** marks what follows from the code without having been run.

**Superseded (2026-11-27):** the plan is now `guest-ram-mapping-list.md`, which replaces option C below.
This note stays for its analysis of upstream's `Uffd` backend, E2B and the options.

## Answer

1. **Upstream's `Uffd` backend can't carry a memory cell's window.** Its handler only *fills* memory
   Firecracker owns: private anonymous memory, filled by `UFFDIO_COPY` or `UFFDIO_ZEROPAGE`. No call
   lets another process put the daemon's pages into the VM process's page tables. Through it, every VM
   holds a private copy of every page it touches (KSM aside, rejected in §4). That is E2B's model, and it loses the sharing of a
   version's pages between the VMs restored from it, which is what the window exists for.
2. **The smallest generic change for fresh boots** is a boot-time memory backend that mirrors snapshot
   load's: `{backend_type: "Uffd", backend_path}` before `InstanceStart`, with the same registration and
   the same handshake. It is small and upstreamable, and it closes a gap upstream has: a booted VM
   can't have an outside handler. On its own it removes no cladefs patch.
3. **The generic change that does remove the fork's guest-RAM code** keeps the window's design and
   moves it into upstream terms: a *lazy* memory backend. The second stage runs in the VM process,
   driven by a table the backend writes, and the private store is a memfd the backend makes. That
   is doc-32's `SHMEM_LAZY` applied to guest RAM regions, offered at boot and at load through point
   2's config as a second `backend_type`. It is one mechanism with plan v3's DAX window (step 4/6),
   and cladefsd serves both.
4. **Rejected: one memfd per version, mapped `MAP_PRIVATE`.** Kernel copy-on-write, no
   `vm.max_map_count` budget, no `EFAULT` retries. But private pages become anonymous memory that the
   vhost-user back-end (plan v3 step 3) can't reach or write. Sharing between versions goes too.
   Details below.

## 1. What upstream's `Uffd` backend gives a handler

From `src/vmm/src/persist.rs` at v1.18.0 (`guest_memory_from_uffd`, `create_guest_memory`,
`send_uffd_handshake`):

- **The uffd.** Firecracker creates it with `UffdBuilder` (`close_on_exec`, `non_blocking`,
  `user_mode_only(false)`). It calls `require_features(FeatureFlags::EVENT_REMOVE)` and nothing else.
  It registers each region with `uffd.register(...)`: the userfaultfd crate's default is
  `UFFDIO_REGISTER_MODE_MISSING` alone. It sends the uffd over `backend_path` with `send_with_fd`,
  then `forget`s the socket.
- **Region layout.** It sends JSON `Vec<GuestRegionUffdMapping>`: `base_host_virt_addr`, `size`,
  `offset` (offsets are contiguous, as in the memory file), `page_size`, and the deprecated
  `page_size_kib`, which is in bytes. The handler finds Firecracker's PID with `SO_PEERCRED`.
- **Memory.** `memory::anonymous(...)`: `MAP_PRIVATE | MAP_ANONYMOUS`, with huge pages if configured.
  No memfd is ever handed over. Upstream uses memfd guest RAM (`memfd_backed`, `MAP_SHARED`) only for
  booted VMs with a vhost-user device, and refuses vhost-user on restored VMs (CHANGELOG #6204, after
  v1.18; `vhost-user-extent-device.md`).
- **Balloon and removes.** `discard_range` (`vstate/memory.rs`) calls `madvise(MADV_DONTNEED)` on
  anonymous memory. The handler gets `UFFD_EVENT_REMOVE` for the range, and the next fault there must
  read zeros. For a `File` restore (private file mapping), it maps fresh anonymous memory over the
  range instead.
- **What the handler can't change.** Features are fixed at `UFFDIO_API`, which Firecracker makes
  (`fs/userfaultfd.c`: `UFFD_FEATURE_INITIALIZED`), so a handler can't add `UFFD_FEATURE_THREAD_ID` or
  the write-protect features later; a second `UFFDIO_API` fails. It *can* change registration:
  `userfaultfd_register` acts on `ctx->mm`, Firecracker's, with no check of the caller. So it can
  register `MISSING|WP` again over a region (registering replaces the modes). It can't register
  `MINOR`: upstream's memory is anonymous, and `anon_can_userfault` refuses `VM_UFFD_MINOR`
  (`mm/userfaultfd.c`). Only shmem mappings take every mode (`shmem_can_userfault`, option B).
  No call maps a file into another process's address space. `UFFDIO_MOVE` fails with `EINVAL`
  unless the caller is the uffd's own mm (`fs/userfaultfd.c`). Remote `process_madvise` allows only
  `COLD`, `PAGEOUT`, `WILLNEED` and `COLLAPSE`.
- **Fresh boots.** None: `build_microvm_for_boot` allocates `allocate_guest_memory()` (anonymous, or
  memfd with vhost-user) and has no backend option.

## 2. What the window needs, and who can do it

Today's window: `cladefs_memory::guest_memory` (fork) → `MemoryCell::window`
(`crates/cladefs_wire/src/memory_cell.rs`) → `CellClient::create_memory_window_in_host_memory`
(`client.rs`) → the second stage, `WindowStage` (`client.rs`'s name for `UffdSecondStage` in
`crates/cladefs_core/src/dax/uffd.rs`). The stage
works *inside* the VM process. It `mmap`s runs of the daemon's arena memfds over guest RAM at their
window offsets, write-protects them, and maps the per-window private store (`PrivateMemory`) shared
where a page is private. "Upstream handler" below means an outside process holding only what point
1 lists.

| Need | Today | Upstream handler | Firecracker change needed |
| --- | --- | --- | --- |
| Share a version's pages between VMs | arena runs `mmap`ed into guest RAM | **no**: it can only copy | the VM process must map pages from files: a lazy stage, or a private file mapping (option B; upstream's `File` backend already does so on restore) |
| Copy-on-write of a shared page | WP fault → copy into the private store | no (nothing shared to copy from) | as above |
| Lazy fill, node at a time, pages installed ahead | `Reach`/`Mapper`, `map_many_warm` + `prefault` | **yes**: `UFFDIO_COPY` a whole node on its first fault (private copies) | none |
| Holes read as zeros | `MAP_ZEROS` runs | **yes**: `UFFDIO_ZEROPAGE` | none |
| Write-protect, writes tracked | `UFFDIO_WRITEPROTECT` | only if Firecracker registers `WP` (or the handler does, point 1) | request `WP` features at `UFFDIO_API` (e2b's fork: `use_sync_wp`, `WP_ASYNC`) |
| KVM async PF: a worker's write taken as a load | `UFFD_FEATURE_THREAD_ID` + vCPU threads (`guest_thread_started`), worker made to fail by a read-only page, vCPU retries `EFAULT` (`AccessFailures`) | no: no thread IDs (feature not requested), no way to make the worker fail (an `mprotect` in Firecracker) | `THREAD_ID`, vCPU thread IDs, the `EFAULT` retry in the vCPU loop |
| `vm.max_map_count` budget | `Mappings`: a share of runs per window, the oldest trimmed | n/a: one VMA per region | only for a stage that maps runs |
| `EFAULT` retries while a mapping is replaced | `AccessFailures` in `vcpu.rs` | n/a: nothing is remapped | only for a stage that remaps |
| Balloon / free page reporting reclaim | `cladefs_memory::reclaim` → `MemoryCell::reclaim` (image runs mapped again) | **yes**: `UFFD_EVENT_REMOVE`, then the next fault served from the image, not zeros (the balloon promises no contents) | none |
| Private pages for a memory-cell snapshot | `private_runs` from the stage, read from guest RAM, `write_sparse`, commit, reclaim | no: anonymous pages in Firecracker. Reachable only with `process_vm_readv` (e2b's `exportMemoryFromFc`) or a memfd handed over (e2b's `use_memfd`) | a memfd the handler can read, or Firecracker writing them (a `Diff` snapshot to a file) |
| Private pages ended after the snapshot | `reclaim` of each run | no: `MADV_DONTNEED` in Firecracker | yes |
| Ephemeral suspend | `snapshot_window_ephemeral` | as the snapshot | as the snapshot |
| Live migration | extent devices' `for_migration`, `release_cell`/`reopen_cell`; memory goes as a version | n/a | the extent device's (plan v3 step 8) |
| Reachable by the vhost-user back-end (plan v3 step 3) | refused today (`CellMemoryError::VhostUser`); plan v3: the daemon makes the private store | no: anonymous memory | memfd guest RAM, or a private store the back-end made |

What the window needs past fill-by-copy is the VM process mapping pages it doesn't own. An outside
handler can't do that. Firecracker has to, so the window's mechanism stays in Firecracker under any
design that keeps sharing. Doing it generically is the change that lets the cladefs patch go.

## 3. E2B

At `75d4fae` (`packages/orchestrator`):

- **A fresh boot uses no UFFD.** `Factory.CreateSandbox` (`pkg/sandbox/sandbox.go`) sets
  `uffd.NewNoopMemory`, and `machineConfigModel` (`pkg/sandbox/fc/client.go`) sends no memory backend.
  Template builds cold boot the first layer this way. At pause the orchestrator reads guest memory out
  of Firecracker with `process_vm_readv` (`exportMemoryFromFc`, `block.NewCacheFromProcessMemory`),
  guided by its fork's `GET /memory` (resident and empty bitmaps) and `GET /memory/mappings`. So boot
  and restore differ in E2B too.
- **A restore always uses UFFD.** `loadSnapshot` sends `MemoryBackend{BackendType: Uffd, UseMemfd,
  UseSyncWp}`. `use_memfd` and `use_sync_wp` are e2b fork fields: guest RAM is a memfd, handed to the
  handler with the uffd, and `UFFD_FEATURE_WP_ASYNC` is left off for synchronous WP faults.
- **The handler** (`pkg/sandbox/uffd/userfaultfd`) never calls `UFFDIO_API` or `UFFDIO_REGISTER`. It uses
  `UFFDIO_COPY` (read faults with `MODE_WP`), `UFFDIO_ZEROPAGE`, `UFFDIO_WRITEPROTECT` and
  `UFFDIO_WAKE`, and refuses `MINOR` faults. `UFFD_EVENT_REMOVE` marks pages `Removed`, which later fault
  as zeros. Free-page hinting is drained before a pause so freed pages become removes, not dirty pages
  (`fph.go`).
- **The dirty set at pause** comes from the fork's `GET /memory/dirty` (a pagemap scan of pages
  written since write-protection was enabled). The bytes come from the memfd when `use_memfd` is on
  (the default, gated on the Firecracker version in `ResumeSandbox`), from `process_vm_readv`
  otherwise. Firecracker's own
  snapshot is always `Full` with no `mem_file_path`. `track_dirty_pages` is never on.
- **No sharing between sandboxes.** Each holds a private copy of every page it touches. The
  orchestrator's cache holds the source once. `vm.max_map_count` is raised to 1,048,576 on hosts.
- **Their fork** is `e2b-dev/e2b-firecracker` (private, `firecracker/fc-versions/build.sh`), v1.17
  with their patches. The repo has no list of them. They also carry memory APIs upstream lacks.

What to take from it: the handler-side tracking (`Removed` pages, hinting drained before a pause) is
what an upstream-handler design needs. Their fork shows the same gaps this note finds: a memfd for
the handler to read private pages, write-protect modes, dirty-set endpoints. Nobody else shares
pages between VMs this way. The async-PF research (`kvm-async-pf-and-shared-guest-memory.md` §5)
found the same.

## 4. Options

**A. Upstream `Uffd`, boot and load (E2B's model).** Firecracker change: the boot-time backend (§5),
plus `WP` features and a memfd to export private pages (e2b's `use_memfd`, `use_sync_wp`). cladefsd
fills by copy. Cost: no sharing. Every VM's touched memory is its own, against "hundreds of forks per
host" whose pages are shared. The async-PF issue goes away (everything is private anyway). The memfd
guest RAM serves vhost-user. **Rejected:** memory density is the point of the window.

**A + KSM.** `PR_SET_MEMORY_MERGE`, set by a launcher, survives exec (`ksm_execve`), so ksmd could
merge identical anonymous pages across VMs with no Firecracker change. **Rejected:** pages are held
twice until ksmd scans them, and scanning costs host CPU in proportion to guest memory. Merged pages
are a known cross-VM side channel, and Firecracker's own docs advise disabling KSM. It doesn't merge
shmem pages, so memfd guest RAM gets nothing.

**B. One memfd per version, mapped `MAP_PRIVATE`, `MISSING` faults filled into its page cache.** The
daemon keeps each version's image as one shmem file laid out as the memory. Every VM restored from
that version maps it privately. On a fault the daemon writes the page into the file (its page cache is
shared) and wakes the fault. Loads of resident pages never reach userspace. Stores copy on write in
the kernel (`do_cow_fault`/`do_wp_page`), with no round trip. `UFFDIO_CONTINUE` on a private shmem
mapping would install the page read-only too (`mfill_atomic_install_pte`: `if (page_in_cache &&
!vm_shared) writable = false`), but `MISSING` alone is enough. Holes are `UFFDIO_ZEROPAGE` (private
VMAs get the zero page). One VMA per region: no `vm.max_map_count` budget and no remap `EFAULT`s.
`MADV_DONTNEED` on the private file mapping drops the copies and reverts to the image, which is
cladefs's reclaim exactly (upstream's `discard_range` maps anonymous memory over it instead, which
would need changing). Async PF: a worker's write on a page not yet filled copies it, as a node's first
page can be today. Filling a whole node keeps the rest shared (**derived**). Upstream's `File` backend is this
option's restore half with no Firecracker change: `snapshot_file` maps the memory file
`MAP_PRIVATE` (`vstate/memory.rs`), so VMs restored from one file share its page cache. It has no
lazy fill (a page not in the page cache is read from the file, which a daemon would have to hold
whole), and a fresh boot could map an empty sparse file. Costs, the same for both:
- Private pages are anonymous memory of the VM process. The vhost-user back-end can't reach them,
  and its writes into guest buffers would have to go into the shared file, which every VM of the
  version maps. Plan v3 step 3 can't serve such a VM.
- Sharing is per version: one shmem page sits at one index of one file, so pages unchanged between
  a version and its parent are held twice when VMs of both run on the host. Today sharing is per
  extent.
- A snapshot needs Firecracker to write the private pages (a `Diff` with `track_dirty_pages`, or a
  pagemap-guided dump: upstream's `mincore` fallback over-approximates on file mappings), then the
  daemon ingests them (`MemoryCell::apply_diff_files` exists). The snapshotting VM keeps its copies
  afterwards: it can't move to the new version's file.

**Rejected** for the vhost-user conflict alone. Reopen if plan v3 changes how the back-end reaches
guest RAM.

**C. A lazy memory backend: the second stage in Firecracker, generic, driven by the backend.**
doc-32's `SHMEM_LAZY` for guest RAM. The backend names sources (fds), writes the page table, and makes
the private store memfd (sealed against resizing), which the VM process maps shared where pages are
private. The VM process installs pages lazily, copies on the guest's stores, keeps its mapping budget,
and retries `EFAULT`. That is today's `WindowStage`, without cladefs's names. What changes hands:
- **To the backend:** filling the table (the `Mapper`'s node mapping moves to cladefsd); reclaim on
  discard (Firecracker sends the range, the backend rewrites the table); and the snapshot itself.
  The private store is the backend's memfd and the private map tells it which pages are private,
  so cladefsd writes them into the cell, commits, makes the version or checkpoints the ephemeral
  cell, and rewrites the table, all while Firecracker holds the VM paused. Firecracker only asks.
- **Stays in Firecracker, generic:** the stage, its vCPU hooks (thread IDs for async PF, the `EFAULT`
  retry), its `vm.max_map_count` share. These are one stage per VM process with the DAX window's
  `SHMEM_LAZY` region (plan v3 step 4), so guest RAM costs a table and a backend channel on top.

Cost: the largest Firecracker proposal of the three. Upstream may refuse it, and then it stays a fork
patch: a generic one, shared with the DAX window's, with no cladefs names. **Recommended:** it keeps
sharing per extent, copy-on-write, private pages the vhost-user back-end reaches, and one path for
boot and restore.

## 5. The boot-time memory backend (the generic piece for fresh boots)

- **API.** `PUT /memory-backend` before `InstanceStart`, and `memory-backend` in a configuration
  file. The body is snapshot load's `MemoryBackend`: `{"backend_type": "Uffd", "backend_path": ...}`.
  `GET /vm/config` returns it. `File` at boot would map an empty sparse file, option B's boot half;
  it is left out, as option B is rejected. With option C,
  `Lazy` is added at both boot and load.
- **Semantics.** Guest memory (DRAM regions, then the virtio-mem region, laid out contiguously as a
  snapshot's memory file is) is created by `create_guest_memory` and registered and handed over by
  the code `guest_memory_from_uffd` runs on load, factored out so both paths call it. The
  handshake's JSON and fd are unchanged, so a handler can't tell a boot from a restore except by
  every page being a hole. Firecracker writes the kernel and initrd into guest memory after the
  handshake, so the handler serves Firecracker's own faults from the start. vhost-user devices are
  refused with a `Uffd` backend at boot, as upstream refuses them on restored VMs (they need memfd
  RAM).
- **Tests.** Upstream's example handlers (`src/firecracker/examples/uffd/`: `on_demand_handler`,
  `fault_all_handler`, `malicious_handler`) serve a booted VM. An integration test boots with
  `Uffd`, snapshots with `Full`, and restores with `Uffd` through the same handler. The `malicious` handler's boot fails cleanly.
- **cladefs.** Nothing uses it until option C's `Lazy` lands, which comes in through the same field.
  Board 14512 (open) would refuse `File`/`Uffd` restores under the cladefs feature, or import them
  into a memory cell. A boot with `Uffd` belongs under the same rule.

## 6. Fork patches: which go, which stay

Once option C's backend, cladefsd serving it, and §5 are in (steps below):

**Go:**
- `src/vmm/src/cladefs_memory.rs` (all 340 lines: `Source`, `Reservation`, `guest_memory`,
  `reclaim`, `snapshot_memory`, `boot_config`).
- `vmm_config/memory_cell.rs`, `PUT /memory-cell`, the `memory-cell` config key, `memory_cell` in
  `GET /vm/config` and `VmmConfig`, and `NoBootMemoryCell` (replaced by `memory-backend`).
- `MemBackendType::Cell`, `MemoryCellConfig` and `diff_files` on load (diff files become the
  backend's business, `Lazy` its type).
- `resources.rs`'s `allocate_cell_memory`, `builder.rs`'s cladefs `allocate_guest_memory` and
  `StartMicrovmError::CellMemory`, `GuestRegionMmap::from_unowned` in `vstate/memory.rs`, the
  `cladefs_memory::reclaim` hook in `discard_range` (replaced by the lazy backend's discard message),
  and the `VhostUser` refusal. The `HugePages` refusal carries over to `Lazy`: the stage maps 4 KiB
  pages.
- `snapshot/create`'s `memory_cell` (`CreateMemoryCellConfig`, the `MemoryCellSnapshot` reply), replaced
  by a generic "persist the memory backend" request whose reply is the backend's JSON, passed through.
**Stay (other threads):**
- `cladefs` crate calls in `vcpu.rs` (`guest_thread_started`, `AccessFailures`) and the `EFAULT`
  logging. The extent device's DAX window needs them as much as guest RAM does. They go once both
  run on the generic stage (plan v3 steps 4 and 6), as that stage's hooks.
- Live migration (`for_migration`, `PUT` release/reopen of a cladefs device, `ExtentCellReleased`):
  the extent device's, until plan v3 step 8 removes it.
- If upstream refuses option C, the lazy backend itself stays as a fork patch, generic, shared with the
  DAX window's `SHMEM_LAZY`. The same goes for the snapshot-to-backend request.
- Unrelated to memory: signal handler stacks, jailer cgroup checks, the vsock and virtio-pci patches,
  `announce_exit`.

Ephemeral suspend and memory-cell snapshots go from Firecracker either way: under option C they are
cladefsd's work behind the generic persist request.

## 7. Steps

Each is a board task under 14602:

1. **Firecracker: boot-time memory backend (`Uffd`)** (board 14609), §5. Proposed upstream. Its own series patch,
   with no cladefs names.
2. **doc-32: guest RAM as a lazy region** (board 14610). The memory backend's channel, the table and sources for
   RAM, the private store from the backend, the private map, discard and persist messages, async PF
   and `EFAULT` as stage hooks. Reviewed in hotbed.
3. **Firecracker: the `Lazy` memory backend** (board 14611) at boot and load (§5's field), the stage generic and
   one per VM process with the DAX window's (plan v3 step 4). Needs 1 and 2.
4. **cladefsd: serve guest RAM as a lazy backend** (board 14612). The `Mapper`'s node mapping, reclaim on discard,
   the private store and map, persist (version or ephemeral checkpoint) daemon-side. Needs 2.
5. **Firecracker: persist the memory backend on `snapshot/create`** (board 14613), the backend's reply passed
   through. Needs 2.
6. **Remove the fork's memory-cell code** (board 14614) (§6, Go). Guest tests boot and restore through `Lazy`.
   Measured against plan v3's step 0 baseline (a fresh boot, a restore, the bench's first touch of
   1 GiB, `b73e6d1ab`). Needs 3, 4 and 5.
