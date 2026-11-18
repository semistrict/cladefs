---
id: TASK-539.4
title: >-
  Counted Dir layer, footprints and the ledger (counting only, nothing reads it
  yet)
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 14:16'
updated_date: '2026-11-18 15:12'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.1
  - TASK-539.3
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: high
type: feature
ordinal: 936000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Requirement (4): count each store's files where the store writes them, above the store page cache, in production and in tests alike. This removes the causes of TASK-520 and TASK-355. (doc-22 task T4.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 HostDir = Counted<CachedDir<DiskDir>>, MemDir = Counted<CachedDir<MemDisk>> and HostFile = CountedFile<CachedFile<DiskFile>>. The impl blocks move with them, storage/image.rs's IoFile path compiles, and every local_cell, cladefs and follower_sim test passes.
- [ ] #2 Dir::files() -> Vec<DirFile{path,len,inode}> exists on DiskDir and MemDisk and is forwarded by CachedDir. It is tested for hard links sharing an inode and for directories not being listed.
- [ ] #3 A process-wide ledger is keyed by DirId. The first opening with no live Counted lists the directory and charges the delta. The last drop keeps only the totals. MemDir::crash reseeds. remove_store_dir forgets the entry.
- [ ] #4 A proptest over MemDir and a HostDir tempdir runs create, write, link, rename, remove, give, replace, settle_for_write, fail_after and crash. After every step, footprint == Σ round_up(len,4096) over the distinct inodes plus directories, and unframed equals the same sum over open-segments/ and open-extents/.
- [ ] #5 A failed inner write raises the count to size(). A write after the last unlink charges nothing. In-place framing (link) counts once. settle_for_write charges up to range.end (test through storage/image.rs's write_file).
- [ ] #6 Ledger tests: a reopen is equal; two HostDirs share one count; an edit made while closed is reconciled at reopen; delete and a failed make return to the earlier count.
- [ ] #7 cell-bench durable and burst on ubu1 (fxjob --bench) are within noise of main.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Dir::entries lists a directory's files (path, len, inode) and directories; DiskDir walks, MemDisk from its tables, CachedDir takes open data files' cached sizes.
2. local_cell::footprint: Footprint (bytes, unframed, charging its HostDisk), FileLen per file shared by names/openings (len counted in granules, names, open-* names), attach/detach (first opening lists and counts in place of what was; last keeps totals), ledger of host stores by (dev, ino), removing() forgets on remove_store_dir.
3. local_cell::Metered<D>: outermost layer, counts create/open/remove/rename (POSIX same-file rename a no-op)/link/replace/create_dir/give, MeteredFile counts writes (a failing one as far as size says) and settle_for_write's range.
4. HostDir = Metered<CachedDir<DiskDir>>, MemDir = Metered<CachedDir<MemDisk>>; MemDisk owns its footprint; Dir::footprint().
5. Oracle tests: random changes over MemDir (failing fs, crashes) and HostDir, footprint == listing after write-back after every step; ledger tests.
<!-- SECTION:PLAN:END -->
