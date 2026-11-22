---
id: doc-26
title: 'Part slabs, quarantine scoped to what was staged, and copy on write in the VM process'
type: specification
created_date: '2026-11-21 20:30'
updated_date: '2026-11-21 20:30'
---
# Part slabs, quarantine scoped to what was staged, and copy on write in the VM process

Design, 2026-11-21: Wave 2, track 2A, of the 2026-11-21 first-principles simplification (board
6124). Nothing here is built yet. Decisions taken without the user are in `OPEN_QUESTIONS.md`
(# Decisions, "doc-26"). The spec is `specs/part-slabs` (PartSlabs.tla). The implementation tasks
are listed under Order of work.

Code is cited by file and exact text (`rg -F` finds it), as of main at 967e93b8a.

## Why

Three pieces of the extent daemon's memory path do much more work than the data asks of them.

**1. A 4 KiB page allocator under a 1 MiB cache.** The extent cache (`crates/cladefs/src/cache.rs`)
brings in and evicts whole parts (`extent_frame::PART`, 1 MiB). It still breaks each part into
256 `physmem::Page`s, from `physmem::alloc_run(self.arena, count, physmem::Wait::Yes)`, a slot
each (`enum Slot`: `Present(physmem::Page),`, `Evicting(physmem::Page),`).
Behind that is `crates/physmem`, 3,543 lines ported from the Zircon kernel's PMM:
- per-page `Frames`;
- a buddy allocator (`pub(crate) struct FreeBlocks`), there to keep runs whole for the VM process's
  mappings (`dead-ends/take-run-from-free-list.md`);
- 2 MiB chunks (`pub const CHUNK_SIZE: usize = 2 << 20;`), each a `MAP_FIXED` mapping of an
  arena's memfd into one physmap, so the daemon's mappings bound physical memory
  (`Some(max.saturating_sub(MAPPINGS_RESERVED) * CHUNK_SIZE as u64)`: 120 GiB at Linux's default
  `vm.max_map_count`);
- `NodeGuard`, which releases chunks after dropping the lock;
- a spare chunk per arena, prefaulted on a thread of its own (`fn populate_later`);
- per-page staged marks.

Memory goes back only a whole free chunk at a time.

**2. One VM stalls every tenant's reclamation.** `quarantine` in
`crates/cladefs/src/cladefsd/server.rs` parks every freed page that any second stage mapped behind
every connected VM process: `for peer in PEERS.get().lock().unwrap().iter() {` …
`peer.acks.park(&pages);`. Each VM process holds the page until it acknowledges its last
taking operation, whether or not it ever mapped the page. So a wedged VM of tenant A holds tenant
B's evicted memory until the watchdog kills it: `UNRESPONSIVE` 5 s, `BEHIND` 60 s, `CATCHING_UP`
600 s, then `process.signal(libc::SIGKILL)`.

**3. Copy on write through the daemon.** Since TASK-501 (board 2352), a private page is the VM
process's own memory (`dax::uffd::PrivateMemory`, `memfd_create(c"cladefs-private-pages"`),
charged to its cgroup. Yet every guest copy on write still goes:
1. the VM process's userfaultfd sends `FAULT` with `FAULT_WRITE`;
2. the daemon runs `Window::copy` (`(Access::Write, Some(_)) => self.copy(page)`), which brings the
   page in, marks it staged, records the offset in `WindowState::private`, and sends a 64-byte
   `STAGE_PRIVATE` per 4 KiB page;
3. the VM process copies the page from the arena and acknowledges it.

A `PRIVATE` request (a `write(2)` of many pages) sends one `STAGE_PRIVATE` per page, which the VM
process coalesces again with a 63-message lookahead (`Receiver::take_privates`,
`const MOST: usize = 63;`). The daemon keeps `private_max` and the set of private offsets only to
tell the VM process what that process already knows.

Two open bugs come from this structure, not from slips:
- **TASK-560 (board 2516).** Concurrent evictions of one cached extent free pages a window still
  maps. Eviction marks pages `Evicting`, unmaps them from the windows, then reaps every `Evicting`
  slot of the extent, another eviction's too (`Slot::Evicting(frame) => Some(frame),`), before
  that other eviction has sent its unmaps.
- **TASK-602 (board 2600).** A MAP of an extent over its own range registers its window again
  (`self.pages.with(Mutex::lock).windows.push(Arc::downgrade(window));`). `Window::place` forgets
  the old mapping before it checks
  `let first = !state.by_source.contains_key(&source_identity(source));`, so each such MAP adds an
  entry.

## Decisions

1. **The cache's unit is a part slab.** An arena hands out 1 MiB slabs (a part's size, at a
   slab-aligned offset of its memfd). A whole part takes a whole slab. A part shorter than 1 MiB
   (an extent's tail, or a small extent) takes a run of pages in a *small slab*. `physmem` goes:
   frames, buddy allocator, chunks, physmap, `NodeGuard`, per-page staged marks. Arenas, caps,
   relief and the quarantine move into one module of `cladefs`, `slabs`, of a few hundred lines.
2. **The daemon maps no arena.** It reads and writes slabs by offset: `pwrite` to fill,
   `pread` to read, `copy_file_range` to promote a part into the default arena (TASK-234),
   `fallocate(PUNCH_HOLE)` to give memory back. The physmap and its `vm.max_map_count` bound on
   physical memory go. Only VM processes map arena memory.
3. **Ownership is in the types.** A part slot owns its slab while present (`Present(PartSlab)`).
   Eviction moves the slab out of the slot into an `Eviction` value the evicting thread owns,
   leaving `Evicting(token)` behind, which holds no memory. Only the owner frees it, after
   sending its own unmaps. No code path can reach a slab another eviction holds, so TASK-560
   cannot happen. A part's window list is a set keyed by window, and inserting is idempotent, so
   TASK-602 cannot happen.
4. **The quarantine waits for the VM processes the slab was staged to, and only those.** Each
   slab records the peers whose windows it was `STAGE_MAP`ped into since it was filled. Freed, it
   waits for each of those peers to acknowledge the last operation sent to it so far, or to
   exit. A tenant's slabs are only ever staged to that tenant's VM processes, so another tenant's
   frozen VM never holds them. A public part's slab waits only for the VM processes that mapped
   it. The watchdog and its SIGKILL stay exactly as they are.
5. **Copy on write moves into the VM process.** `UffdSecondStage` copies a written page from the
   arena slab it has staged at that offset (or zeros), counts it against `private_max`, and
   tells the daemon nothing. These go: `FAULT_WRITE`, `STAGE_PRIVATE`, `STAGE_PRIVATE_COUNTS`'s
   private half, `Receiver::take_privates`, `Window::copy`, `Window::make_private`,
   `WindowState::private`, `Pte::Private`, `CopyOnWrite` in the daemon, and `PRIVATE` as a
   request. The device's `PRIVATE` is served in the VM process.
6. **A host's change keeps private pages; only the guest's own MAP or UNMAP ends one.** The
   daemon no longer knows which pages are private, so it sends an eviction's unmap of every page
   it staged a slab at as an *evict*: `STAGE_UNMAP` with a new flag, `STAGE_KEEP_PRIVATE`. The
   VM process skips a private page under an evict. A guest MAP or UNMAP sends a plain
   `STAGE_UNMAP`, which ends a private page as today.
7. **The VM process gets both arenas read-only.** Today the tenant's arena is handed over
   writable (`memory: Arc<std::os::fd::OwnedFd>`, beside
   `let public = reopen_read_only(crate::pages::physical_memory())?;`). Nothing writes it since
   TASK-501. Read-only, a VM process can no longer write the extents its tenant's other VMs share.
8. **Wire and formats change outright.** `wire::VERSION` is bumped and the old messages are
   refused, with no shim. No bucket or on-disk format changes.

## Part slabs

### The allocator (`cladefs::slabs`)

An **arena** is one shared memory object (a memfd on Linux, `MFD_ALLOW_SEALING`, sealed
`F_SEAL_SHRINK | F_SEAL_SEAL`, mode 0600, as `create_shared_memory` makes it today). It is a
sequence of slab slots of 1 MiB each. Its logical size is its high-water slot count times 1 MiB,
grown with `ftruncate` and never shrunk, which costs no memory. The arena holds:
- `slots: Vec<SlotState>`, indexed by slab number. Each slot is free (punched), spare
  (populated, unused), whole (one part), or small (a 256-bit page map of runs);
- `free: BTreeSet<u32>`, the free slots, taken lowest first so that the object stays compact;
- `spares: SmallVec<[u32; 2]>`, populated slots kept ready;
- `smalls`: the small slabs with room, by their largest free run;
- `staged`: per allocated run (a whole slab's, or a small slab's run, keyed by slot and first
  page), the peers it was staged to (Quarantine, below), emptied as the run is allocated. Not per
  slot: a small slab holds several parts' runs, and a run allocated in it must not forget the
  peers another of its runs was staged to;
- `cap: Option<u64>` in slabs, as `set_arena_cap` has it in chunks now.

The total cap and the arenas' relief stay where they are: `Starved::{Arena, Total, Removed}`,
waiting allocations woken by a pulse, and `wants_relief` skipping arenas still owed quarantined
memory (TASK-161).

The handle is `PartSlab { arena: Arc<Arena>, run: Run }`, where `Run { slab: u32, first: u16,
pages: u16 }`, and `first = 0, pages = 256` is a whole slab. It is not `Clone`. Dropping it frees
the run: at once, to the free set or to the small slab's map, if no peer was staged it, or else
into quarantine. It is the only way to free a run, so an owned value is the only thing that can.

- **Allocating a whole slab** takes a spare if there is one, else the lowest free slot, which it
  populates (`fallocate(fd, 0, slab, 1 MiB)`). The arena keeps `SPARE_SLABS = 2` spares,
  repopulated on a thread of its own as they are taken. That is today's spare-chunk prefault,
  which took first-touch allocation, memcg charging and zeroing off the page-in's path
  (`dead-ends/populate-chunks-in-one-madvise.md`, `dead-ends/idle-chunk-cache.md`; 99 µs to
  40 µs for the daemon's mapping). Two spares are 2 MiB, as the one 2 MiB spare chunk was.
- **Allocating a run of k pages** (k < 256) takes first-fit from the small slab with the
  smallest largest-free-run that still fits, else turns a new whole slab small. The run's pages
  are contiguous in the memfd, so the VM process maps a part with one mapping either way.
- **Freeing a whole slab** keeps it as a spare while there are fewer than `SPARE_SLABS`, else
  punches it (`fallocate(PUNCH_HOLE|KEEP_SIZE)`) and returns it to `free`.
- **Freeing a run** punches its pages at once, page-granular, and clears them from the map. An
  empty small slab becomes free. So no small slab holds memory nothing uses.

**What the governor counts** (`memory.rs`'s `governed_bytes`): an arena's bytes are its whole
slabs and spares times 1 MiB, plus its small slabs' allocated pages. These are exact, as the
memory is the memfd's pages: populated slots are counted, punched ones aren't. Everything stays
under the one budget (TASK-433, board 2094). The host's page cache isn't involved: shmem pages are
the arena's memory, as they are today.

**Relief** asks an arena for a slab at a time (`RELIEF_BYTES`, 4 MiB today, becomes 4 slabs).
The governor's shrink evicts, then `release_spares` punches spares. Both are as today, in slabs.

**macOS** (development only) backs an arena with an unlinked file in the process's temporary
directory, read with `pread`/`pwrite` and punched with `fcntl(F_PUNCHHOLE)`, rather than
`shm_open`, which supports neither `pread` nor punching. One code path serves both platforms.
The macOS file's pages are the host's page cache. That breaks the one-memory-budget rule, but
only on a development machine, where no governed budget is measured. Production is Linux.

### The cache (`cache.rs`)

`CachedExtent` holds a `Vec<PartSlot>`, one slot per part rather than per page:

```rust
enum PartSlot {
    Absent,
    Filling,                       // its page-in's waiters wait on `changed`
    Present(PartSlab),
    Evicting(EvictionToken),       // a number, no memory: the slab is the evicting thread's
    Failed(Errno),
}
```

A page-in allocates one `PartSlab` for the part (whole, or a run for a short part), decompresses
into a buffer, and `pwrite`s it to the slab. Read-ahead (`PARTS_AHEAD`) is unchanged. A read
through the cache (`CachedExtent::read`, a guest's `READ` of something mapped nowhere) uses
`pread` on the slab. Promotion of a tenant's copy into the default arena (TASK-234) is
`copy_file_range` from the slab to a new default-arena slab, with no bounce through the daemon's
memory.

**Eviction** has the same three steps, but each step is owned:

1. Under the extent's lock: `let PartSlot::Present(slab) = mem::replace(slot,
   PartSlot::Evicting(token))`. The slab moves into `Eviction { slab, token }` on this thread's
   stack. Faults on the part wait (`Filling | Evicting` wait, as now).
2. Without the lock: each window in the part's window set sends an evict of every page it staged
   the slab at (`STAGE_UNMAP | STAGE_KEEP_PRIVATE`). The spec's eviction finds its pages through
   the part's window list too, as `unmap_pages_of` does.
3. Under the lock again: the slot is `Evicting(token)` with this thread's token, so it becomes
   `Absent`. Then `drop(eviction.slab)`, which quarantines it (below).

A concurrent eviction of the same part finds `Evicting` and returns: there is nothing for it to
evict. An eviction of another part touches only its own part's slot. Nothing scans for
"every evicting slot" because no slot holds an evicting slab. That is the structural fix for
TASK-560. Its acceptance test, two evictions interleaved by Shuttle with a test quarantine that
asserts no second stage maps a freed run, becomes a Shuttle model of `slabs` and `cache` together
(Order of work, step 2).

**The window set** becomes `windows: HashMap<WindowId, Weak<Window>>`. `CachedExtent::mapped`
inserts and `unmapped` removes, both idempotent. `Window::place` computes `first` before it
forgets the range it replaces. Either change alone fixes TASK-602; the set makes it impossible,
which no ordering inside `place` could.

### Extents shorter than a slab, and tails

A part's length is `min(PART, extent's end - part start)`, less the first part's phase
(`part_of`). A part of k pages takes a run of k pages of a small slab. Most extents in a small-file
workload (pgbench's hundreds of thousands of 8 KiB extents) are such runs. They cost what they
cost today: their own pages, mapped by the VM process one run each. What changes is the
allocator, a page map per small slab instead of a buddy allocator over the whole of physical
memory. A small slab's free pages are punched at once, so fragmentation costs memfd offsets,
which cost nothing, and no memory.

### What the VM process maps

A VM process maps arena memory at the window's offsets, one mapping per run that is contiguous in
both the window and the arena (`dax::uffd`'s runs, `budget(max)`). A whole part's slab is
contiguous, so a region mapping an extent of N parts installs at most N mappings, where today's
runs averaged 27.7 KB (`dead-ends/take-run-from-free-list.md`, after the buddy allocator's fix).
That cuts the VM process's mappings, and so its run budget's trimming, by up to 37x for large
extents. Small extents map as today.

### Address space

The daemon maps nothing, so it reserves no address space. Each VM process maps only what its
window installs. An arena's memfd offsets run to its high-water slab count times 1 MiB, which
costs nothing. Its slot count is bounded by its cap plus the small slabs' slack: at most one
partly used small slab per run size in use. Without a cap, the bound is the total cap. Today's
`physmem::configure` refuses a memory size past `max_size()`. That check goes, and with it the
setup step of raising `vm.max_map_count` on hosts of more than 120 GiB (TASK-79.37). The VM
process's run budget still follows `vm.max_map_count`, as it does today.

## Quarantine scoped to what was staged

A run was mapped into a VM process if, and only if, its window was sent a `STAGE_MAP` naming it.
`Window::present` and `map_run` send it under the window's lock. At that point the window adds
its peer to the run's entry in `staged` (`StagedTo`, a `SmallVec<[PeerId; 4]>` behind the
arena's lock, set-like), the run's own, not its slab's. This replaces `frame.mark_staged()`.

Freeing a `PartSlab` whose `staged` is not empty creates `Quarantined { slab, waits }` with
`waits = { (peer, peer.acks.sent()) : peer ∈ staged, peer alive }`, then parks it on each of those
peers alone: `peer.acks.park_until(id, quarantined.clone())`. It frees once the last peer has
acknowledged its id, or closed after its process exited.

**Why this is enough.** Every operation that gave a peer access to the run (a `STAGE_MAP`), and
every operation that took it away (a guest MAP's or UNMAP's `STAGE_UNMAP`, an eviction's evict),
was sent before the free, because the eviction sends its evicts before it drops its slab, and a
guest's unmap was sent earlier still. Each peer makes its operations in order. So once a peer has
acknowledged the last operation sent to it before the free, it no longer maps the run. A peer it
was never staged to never mapped it. An honest VM process doesn't `mmap` its arena outside what
it was staged. A hostile one can read its own tenant's arena, or the public one, whenever it
likes, and gains nothing a quarantine could deny it: arenas are per tenant, and a punched or
refilled run of its own arena holds its own tenant's data or zeros. The quarantine is for
correctness (an honest guest must never read a reused run through a stale mapping), not for
confidentiality, which the arena boundary provides.

**Tenant isolation follows.** Tenant B's runs are only ever staged to tenant B's VM processes,
because a window only maps the arenas its VM process holds (`Arenas::of`). So no wait on a
tenant-B run names a tenant-A peer. A frozen tenant-A VM process holds back only what it was
staged: its own tenant's runs, and the public runs it mapped.

**Frozen and killed VM processes.** Unchanged: `watch()` lets go of a peer silent for
`UNRESPONSIVE`, behind for `BEHIND` or catching up for `CATCHING_UP`, and `Peer::gone` sends
SIGKILL through its pidfd, waits for the pidfd to become readable (the process gone), and only
then `peer.acks.close()` drops what the peer held. The spec's `killBeforeLetGo` rule shows why the
order matters: letting go of a stuck process's waits before it is dead lets it thaw with a freed
run mapped.

**What it costs.** One `SmallVec` push per `STAGE_MAP` of a run not yet staged to that peer, done
under the window lock the operation already holds. Parking touches only the staged peers rather
than every peer on the host.

## Copy on write in the VM process

### What the VM process knows already

`UffdSecondStage` tracks the window page by page (`PageChanges`): what each page is named
(`mapped: Option<Mapped { phys, installed, used }>`, from `STAGE_MAP`'s arena and offset),
whether it is installed, withheld or restricted, and whether it is private (into
`PrivateMemory`). It makes every operation under its `pages` lock, the same lock its fault path
takes. So it already has everything a copy needs: where the page's bytes are (its arena's fd and
offset), and its own private memory to copy them into.

### The fault path

A guest write to a page of a copying window reaches `resolve` as a write-protect fault
(`UFFD_PAGEFAULT_FLAG_WP`) or a missing fault. In this order:

1. **Under `pages`:** if the page is private, it is already writable, and this is a spurious fault
   (resolve as a wake). If it is named with a known arena location, and this is a vCPU's write
   (`guest_writes(thread)`, `GUEST_THREADS`), take the copy: if the private count is at
   `private_max`, refuse (the VM stops, exit 159, as `EACCES` from the daemon does today);
   otherwise `pread` the 4 KiB from the arena fd at the page's location (or zero it, for the zeros
   page), `PrivateMemory::fill` it (`pwrite`), map the private memory there writable
   (`base_memory`, as `make_private` does today), count it, prefault it, and wake. Still under
   `pages`, mark the page private and its staged mapping gone.
2. **Named, not in memory** (`STAGE_MAPPED`, the daemon's page-in pending) **or not named:**
   send a plain `FAULT` (read) to the daemon, which brings the part in and stages it (a
   `STAGE_MAP`), then retry step 1. `FAULT_WRITE` goes from the wire. A write fault that finds
   nothing named still fails as today (`EFAULT`, the guest's own error, 159).
3. **KVM's async page fault worker** (x86, `async_pf_execute`, not in `GUEST_THREADS`): resolved
   as a load, as now (`restrict_for_worker`). The vCPU's own retry decides, as now. Withheld pages
   stay withheld until the window maps them anew (`mapping_gives_back_withheld`), so a worker's
   re-fault of a page the guest unmapped still fails with `EFAULT`, which KVM drops
   (`dead-ends/unmap-maps-zeros.md`: no-kvmapf does not disable KVM's async page faults).

The copy reads the arena under `pages`, the lock the receiving thread takes to make an evict. So
the evict comes after the copy, its acknowledgement after that, and only then may the daemon free
the run (spec rule `copyUnderLock`; without it TLC finds a copy of a freed and punched run,
`NoStaleCopy`).

**`PRIVATE`** (the device request a `write(2)` of many pages sends) is served by the VM process:
`virtio_extent`'s `make_private` runs the same copy for each page of the range under one `pages`
lock, one `PrivateMemory::fill` per run, then one `mmap` and one flush. Pages not yet in memory
are faulted first with one `FAULT` per part, read. The run coalescing that `take_privates`
rebuilt from 64 messages is now a loop over the range.

### Operations that change a page

| Operation | Sent when | A private page there |
|---|---|---|
| `STAGE_MAP` | the daemon stages a run at a page its `ptes` say nothing is at: the faulting page, and the rest of the part with it (`Window::resolve`'s `install`) | stays (spec rule `mapsKeepPrivate`). After an evict left a private page, the daemon's `ptes` stage nothing there, so the next fault on another page of the part maps over it; the daemon cannot tell, the VM process can |
| `STAGE_UNMAP` | a guest's `MAP`, `MAP_ZEROS` or `UNMAP` over the page | ends it, as the guest replaced what it wrote (rebind, punch, truncate) |
| `STAGE_UNMAP \| STAGE_KEEP_PRIVATE` | an eviction, a free's zeros (`Private::Keep` today), any host change | stays; only a staged mapping there goes |
| `STAGE_MAPPED` | the window names pages anew | stays named; the page's new name is used when the guest next maps over it |

`MAP_ZEROS` stays a `MAP` flag. The daemon maps the shared zeros page, and the VM process
recognises its location from the `OPEN` reply (a new `args` field) to copy zeros with no `pread`.

### `private_max`

The VM process enforces it (`PrivateMemory`'s count). The daemon's `WINDOW` request no longer
carries it: `args[1]` goes, and the device sets it in its configuration space from
`dax_private_max_bytes`, as `private_max(len, configured)` does today. A VM process that ignores
its own limit only spends its own cgroup's memory, which already bounds it (TASK-501). So this
was never a trust boundary, and moving it leaves nothing unchecked that mattered.

### What the daemon stops knowing, and who asks

- **`OpenCell::private_pages`** and its users:
  - `OpenCell::at_rest` refuses a quiescent fork or a version with private pages, with "the DAX
    window holds {private} private pages, whose bytes no extent holds". That check moves into
    the VM process, which is the only caller that can have private pages. `FORK` with
    `FORK_QUIESCENT` is a VM process's request. `Device::take_snapshot` checks
    `UffdSecondStage::private_pages()` after `quiesce()` and refuses `SnapshotError::Private(n)`
    before `make_version`.
  - The control API's quiescent forks take a cell's committed state, which private pages are not
    part of. Their window mappings are not recorded, so the TASK-203 hazard (a restored VM reading
    a page's old bytes) cannot arise there.
  - The daemon-side check was never a trust boundary either: a VM process lying about its private
    pages corrupts only its own snapshot.
- **`window_mappings()`'s `EBUSY` while private** (`Some(dax) if dax.private_pages() > 0 =>
  Err(Errno::EBUSY)`) moves the same way: `take_snapshot` checks before asking.
- **Gauges** (`Gauges::private_pages`, the daemon's per-VM metrics, TASK-111.5): reported by the
  VM process with its stage counts. `STAGE_COUNTS` gains a private-pages field and
  `STAGE_PRIVATE_COUNTS` goes. It is advisory, as metrics are.
- **`eviction_skips_private`** (a `dax` rule): replaced by the VM process's handling of
  `STAGE_KEEP_PRIVATE` (spec rule `evictionKeepsPrivate`).

### VM snapshots: freeze, then restore

`Device::take_snapshot` runs on a paused VM whose guest filesystem is frozen. The guest's freeze
rebinds its private pages (doc-13), so the VM process's count is final and normally 0. If it is
not 0, the snapshot is refused before the version is made, as today, only decided in the VM
process. `map_again` on restore sends `MAP_MANY`, as today. A restored window has no private
pages, since none were snapshotted. Nothing else changes: `dead-ends/lazy-restore-of-window-mappings.md`
stands (restores install eagerly), and so does `populate-chunks-in-one-madvise.md` (no
population on a request's path).

### The dead ends this respects

- `deferred-stage-unmaps.md`: no unmap is deferred or batched outside the window lock. Evicts
  and unmaps are sent as today, in order, with their acknowledgement IDs in order.
- `lazy-unmap-of-freed-window-space.md`: unchanged. A guest UNMAP still sends `STAGE_UNMAP`. A
  host change that replaces a mapping still unmaps first, and a page costs one TLB shootdown
  however it goes.
- `unmap-maps-zeros.md`: unchanged. Withholding stays, and with it the async-PF behaviour.
- `lazy-restore-of-window-mappings.md`, `populate-chunks-in-one-madvise.md`,
  `idle-chunk-cache.md`: honoured above (eager restore; spares populated off the path).

### Windows in the daemon's own process

Tests and `pagein-bench` make windows in the calling process (`create_dax_window(…, None)`,
`Translation::Host`), about 40 of `open_cell`'s tests among them. Their `map_run` maps the arena
into the process already (`crate::pages::arena_memory(location.arena)`), as a VM process does, so
Decision 2 leaves it as it is. Their copy on write read the daemon's physmap
(`Held::Bytes(unsafe { std::slice::from_raw_parts(frame.as_ptr(), ps) }…)`); with no physmap,
and with `Window::copy` gone, an in-process window copies as the VM process does: the same
function (`pread` of the arena at the page's location into the window's private memory, under the
window's lock), called from the in-process translation rather than from `dax::uffd`. One copy path,
so the tests run what VM processes run (the rule that tests run production layers).

## The spec: `specs/part-slabs`

`PartSlabs.tla` models two tenants' VM processes (va, vb), their arenas (A, B) and the default
one (D), a part of each, one slab each, two eviction threads, and the guests' MAPs, UNMAPs, faults
and writes. Each VM process makes its operations in order and copies on write itself. It may
freeze, thaw, or be killed. A slab may be punched as it is freed.

Invariants:
- `NoStaleInstall`: no live VM process maps a slab that no longer holds the part it was staged
  as. This covers no post-free exposure, and no cross-tenant exposure, since slabs are refilled
  only within their arena.
- `NoStaleCopy`: a private page holds the bytes the window named as the write began.
- `NoLostWrite`: no guest write is lost except to its own MAP or UNMAP, or its process's death.
- `ArenasHeld`.
- `ScopedWaits`: a quarantined slab waits only for VM processes it was staged to.
- `WindowsOnce`.
- `PrivateWithinMax`.
- `OneOwner`: each slab has exactly one owner, which knows it, so none is leaked or owned twice.
- The property `QuarantineDrains` (PartSlabsLive, fair): every quarantined slab is freed in time,
  whatever freezes.

| Rule | Mutation | What TLC finds without it |
|---|---|---|
| `ownTokens` | NoOwnTokens | TASK-560: one eviction frees the other's slab before its evict; the VM process acknowledged everything, the slab is punched under it (`NoStaleInstall`) |
| `waitsForStaged` | NoWaitsForStaged | quarantine by the arena's tenant: a public slab staged to va waits for no one (`NoStaleInstall`) |
| `scopedWaits` | NoScopedWaits | PEERS-wide quarantine: tenant A's slab waits on vb (`ScopedWaits`) |
| `killBeforeLetGo` | NoKillBeforeLetGo | waits let go of a frozen VM process that then thaws (`NoStaleInstall`) |
| `copyUnderLock` | NoCopyUnderLock | an evict made, acknowledged and the slab punched between the look and the copy (`NoStaleCopy`) |
| `evictionKeepsPrivate` | NoEvictionKeepsPrivate | an eviction ends a private page (`NoLostWrite`) |
| `mapsKeepPrivate` | NoMapsKeepPrivate | the page-in after an eviction maps the rest of the part over a private page (`NoLostWrite`) |
| `registerOnce` | NoRegisterOnce | TASK-602 (`WindowsOnce`) |
| `watchdog` | NoWatchdog | a frozen VM process holds its slabs for good (`QuarantineDrains`) |

Models: PartSlabs (453,468 states), PartSlabsAround (two pages a window, faults installing the
rest of the part: 248,274), PartSlabsLive (85,724), and PartSlabsWide, deep (two pages a window,
three MAPs, two evictors, freezes: 99 million states, about 15 minutes, before fault-around was
added). One part an arena: a slab refilled with another part's bytes is modelled by the punch's
zeros, which stand for any bytes but the part's. `ArenasHeld` and `PrivateWithinMax` hold by the
model's construction and are kept as checks. specs/dax-cow keeps the guest's side
(dirty sets, writeback, rebinding, install-as-named), which this spec takes as given.

## Failure modes

- **The daemon crashes:** VM processes lose their daemon (`CladefsdLost`) and stop, as today.
  Arenas die with the daemon. Private pages are the VM process's and die with it.
- **A VM process crashes or is killed:** its pidfd becomes readable, `Peer::gone` closes its acks,
  and its waits are let go of. Its runs free.
- **A VM process stops answering:** the watchdog, as today.
- **`pwrite` or `fallocate` fails on a fill** (`ENOSPC` in shmem, memcg): the page-in fails
  (`Failed(Errno)`), its waiters are told, and the governor's relief runs, as an allocation
  failure does today. It fails hard, with no fallback to the host's page cache.
- **A run's punch fails:** an error logged and counted, and the run kept as not free, so that its
  memory is still counted. This should never happen on a memfd. A hard failure here would take
  down every tenant's daemon, so the run is leaked visibly instead (a metric, with a task filed
  if seen).

## Composition with Wave 1

None of Wave 1's tasks (board 6011, 6014, 6017, 6020, 6023, 6026, 6029) touches `physmem`, `cache.rs`, `dax.rs`,
`dax/uffd.rs`, `server.rs`'s quarantine or `wire.rs`'s stage messages. Board 6026 (unify
`framed::Unpacked` and held parts) and board 6029 (an A/B of `see_ahead` and `HeldParts`) touch
the governor's accounting of parts held, not arenas. Step 1 below rebases on 6026's accounting if
it lands first. No ordering is required.

## Order of work

Each step is a board task, unclaimed, and lands on main with the workspace and guest suites
passing.

1. **`cladefs::slabs`: arenas of part slabs** (replaces `physmem`). Covers `Arena`, `PartSlab`,
   `Run`, small slabs, spares, caps, relief, the governor's counts, macOS's file backing, the
   loom/Shuttle models of `slabs` alone, and `physmem`'s crate and its tests deleted (with the
   user's OK, as they are tests; `OPEN_QUESTIONS.md`).
2. **The cache in part slots with owned eviction** (`cache.rs`). Covers `PartSlot`, `Eviction`,
   the window set, `Window::place`'s `first`, and the TASK-560 Shuttle reproducer and TASK-602's
   1,000-MAP test, each red against main. Closes board 2516 and 2600. Needs 1.
3. **Quarantine scoped to staged peers** (`server.rs`, `slabs`). Covers `StagedTo` set at
   `STAGE_MAP`, `Quarantined { waits }`, and `Acks::park_until`. Tests: a frozen tenant-A VM
   process does not delay a tenant-B shrink. Needs 1.
4. **Copy on write in the VM process** (`dax/uffd.rs`, `dax.rs`, `wire.rs`, `virtio_extent`). Covers
   the local copy under `pages`, `PRIVATE` served locally, `STAGE_KEEP_PRIVATE`, `wire::VERSION`
   bumped, the deletions of Decision 5, the snapshot check in `Device::take_snapshot`, the zeros
   page's location in `OPEN`, and both arenas read-only. Tests: the dax Shuttle tests extended to
   a fault racing an evict of the page it copies (`copyUnderLock`'s class), a page-in after an
   eviction installing the rest of a part over a private page (`mapsKeepPrivate`'s), in-process
   windows copying through the same function, and the hostile
   proptest's `PRIVATE` storms against the VM process's bound. Needs 2 (eviction's evicts).
5. **Measure.** The nightly's `bench-fs-fxfs-guest-dax`, and fork-cold and fork-warm, A/B against
   the commit before step 1. Expected: fewer VM-process mappings and stage maps per MiB, no copy
   round trips, daemon CPU per MiB down. A regression is filed and fixed, not explained.

Steps 2 and 3 may go in parallel after 1. Step 4 may start in parallel and lands after 2.
