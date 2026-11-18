---
id: TASK-456.3
title: >-
  Guest memory restored as a copy-on-write window over a memory volume, its
  pages shared between VMs
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 13:33'
updated_date: '2026-11-18 01:45'
labels:
  - memory
  - firecracker
  - dax
dependencies:
  - TASK-456.2
parent_task_id: TASK-456
priority: high
ordinal: 864000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Firecracker's restore today fills guest memory from a file (MAP_PRIVATE) or through an outside UFFD handler (UFFDIO_COPY): each VM a copy of every page it touches. The DAX window's second stage (dax/uffd.rs, run in the VM process, UffdSecondStage::in_process) already maps sealed extents' pages from cladefsd's memory read-only and gives a VM a private copy of a page it stores to (doc-13). Guest memory restored from a memory volume should be served the same way, so VMs restored from one snapshot share every page none of them wrote. Two limits of the window don't fit guest memory: private pages capped at dax_private_max and a VM past it stopped (exit 159), where a guest may write all its memory; and one host mapping per run of pages, of which Linux allows vm.max_map_count (65,530 by default), against which the stage keeps a budget (TASK-12): a 64 GiB guest over scattered extents passes it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Two VMs restored from one snapshot on one host share the pages neither wrote: the host holds one copy of each, shown by a guest test reading the pages in both and counting cladefsd's pages and the VMs' private pages
- [ ] #2 A guest may write all its memory: private pages bounded by the guest's memory, charged to the VM process as its guest memory is (nothing reserved against the daemon or the tenant, TASK-501), not by dax_private_max, shown by a guest test writing every page
- [ ] #3 A 64 GiB guest restores and runs within vm.max_map_count, shown by a test
- [ ] #4 Pages the guest gives back through the balloon's free page reporting drop their private copies, shown by a test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Daemon: a copying window of a memory volume's store (its description's kind) reserves nothing in the tenant's arena and holds up to the window's pages as private ones. Test: a memory store's window past the tenant's arena is made; an Fxfs store's is refused ENOMEM.
2. extent_files: Volume::runs(name, offset, len): what the file holds there as of its last commit, in runs (extent and offset, zeros, or past its end), the tree's nodes read as needed. Not the volume's own window machinery: a memory window's pages turn private as the guest writes, which the volume's reads through its window would take for committed bytes.
3. cladefs: RemoteExtentStore window in host memory with a resolver of faults on what isn't mapped yet; MemoryVolume::window(base, size, prefault): a copying window over the memory, of all its pages, whose fault on a page of a tree node not mapped yet maps that node's runs first (map_many), on a thread of its own (the volume's calls block), the faults awaiting it, one mapping per node however many fault on it. Daemon tests: a window over a fork reads the parent's pages through faults; two windows over two forks share the daemon's pages (pages cached counted once); a page stored to turns private in its window alone.
4. Firecracker (cladefs feature): restore from a memory volume: guest memory one reservation, its regions carved from it, KVM slots as before; the VM's memory a fork of the snapshot's memory point (OPEN_FORK_FROM), its window over the whole of it; snapshot/load's mem_backend gains the volume (point_on_host, path_on_host, extentd_socket). cladefsctl imports a Firecracker memory file into a memory volume (zero pages holes), to make one from today's snapshots.
5. Guest tests on ubu1: two VMs restored from one imported snapshot run and share the pages neither wrote (extentd's cached pages against the VMs' private pages); a guest writing all its memory runs on; balloon free page reporting drops private copies; a 64 GiB guest within vm.max_map_count.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decided with the user 2026-11-15: (1) a restored VM's memory is mapped on the guest's first fault on an unmapped page: the runs of the tree node around it (170 pages) mapped, then the page installed; restore reads only the root. (2) extentd's per-window state stays as it is for memory windows (runs, plus an entry per page installed and per private page, which eviction needs); the 750 MiB of MAX_WINDOW is a flow-control allowance, not memory held. (3) A memory window reserves nothing in extentd: its private pages are the VM process's memory, limited by its cgroup.

Steps 1-3 (branch t456-3, b0b11a1b): memory volumes' copying windows reserve nothing (CopyOnWrite::unreserved, by the store's kind); extent_files Volume::runs/node_bytes; RemoteExtentStore::create_memory_window_in_host_memory with an Unmapped resolver (DAX windows' faults unchanged, sent eagerly as before); MemoryVolume::window (node bitset, a mapper thread, one mapping per node) and private_runs (the stage's private memory's SEEK_DATA/SEEK_HOLE). ubu1: 248 extent_files/extentd/dax tests pass, among them memory_windows_map_what_faults_reach_and_share_their_pages (two forks' windows read the image through faults, holes zeros; the same page of both maps the daemon's one page; a guest store makes a page private in one window alone, private_runs [5 pages..6 pages], the other none).

Step 4-5 (branch t456-3, ubu1): VMs restore from a memory volume and run. Fixed on the way: (1) a memory window counts any VM-process thread's write as a store (Firecracker's main thread wrote guest memory restoring devices: SIGSEGV), tgkill(pid, tid, 0) telling them from others', seccomp rules getpid and tgkill of signal 0; (2) a worker's write to a page installed write-protected is a store: KVM hands a store waiting on userfaultfd to x86's async page fault worker (kworker events), whether the guest enabled async PF or not (clearing MSR_KVM_ASYNC_PF_EN changed nothing, dropped), and taking every worker write for a load refused the store over and over (exit 159). fxfs-guest-snapshot-memory-volume-dax passes (CLADEFS_EXTENTD_MEMORY=3 GiB, CLADEFS_DAX_WINDOW_MIB=2048, TASK-442): a 512 MiB VM's memory file imported in 3.4 s, two VMs restored from its point one after the other, 174 ms and 769 ms, each run to its end; the daemon held 2 MiB -> 80 MiB after the first, -> 84 MiB after the second: the second took 4 MiB, the first 78 MiB (pages shared). fxfs-guest-snapshot-dax passes beside it.

2026-11-16, after the research (docs/research/kvm-async-pf-and-shared-guest-memory.md) and the user's choice of installing ahead: a node's pages are read into the daemon, mapped and installed as its first page faults (MemoryVolume's mapper: read, runs, map_many, prefault), and every write to a memory window is a store (writes_are_stores; the tgkill rule, its seccomp commit and the restrict-and-lift dance for memory windows gone). Trace: kworker first touches of pages not installed 7,608 -> 125. fxfs-guest-snapshot-memory-volume-dax and fxfs-guest-snapshot-dax pass (ubu1, CLADEFS_EXTENTD_MEMORY=3 GiB, CLADEFS_DAX_WINDOW_MIB=2048): a restored in 112 ms, its windows 32,991 pages at most, 11,148 private (its stores; 66% shared); the daemon 2 -> 86 MiB; b restored in 90 ms, wrote 347 MiB of free memory (99,850 private pages, no refusal), the daemon +8 MiB. AC1 and AC2 shown; AC3 (64 GiB) and AC4 (balloon) open.

2026-11-16, rebased onto main (t456-4, 05356ff6; the renames: memory vdisks, cladefsd, CLADEFSD_MEMORY, GetStats): macOS clippy + workspace 2,549 pass; ubu1 273 extent_files/sqlite_extents/cladefs tests pass; fxfs-guest-snapshot-memory-vdisk-dax and fxfs-guest-snapshot-dax pass (CLADEFSD_MEMORY=3 GiB, CLADEFS_DAX_WINDOW_MIB=2048): a 32,481 pages at most, 10,821 private (67% shared), the daemon 2 -> 90 MiB; b 99,353 private of 121,051, the daemon +8 MiB.

2026-11-17, rebased onto main again (t456-4, 99ef3542; TASK-501 made every copying window reserve nothing, so CopyOnWrite::unreserved went; Firecracker's commit onto the series' bac96b04 as 3c1b2d9a, its fields cladefsd_socket beside cladefs_overrides): macOS clippy + workspace 2,645 pass; ubu1 273 of 274 extent_files/sqlite_extents/cladefs tests pass, cladefs::isolation's ENOMEM under host load passing alone (TASK-510); fxfs-guest-snapshot-memory-vdisk-dax and fxfs-guest-snapshot-dax pass (a guest kernel rebuilt for the module's rename): a 32,991 pages at most, 10,841 private (67% shared), the daemon 2 -> 86 MiB; b 95,999 private of 117,991, the daemon +10 MiB.
<!-- SECTION:NOTES:END -->
