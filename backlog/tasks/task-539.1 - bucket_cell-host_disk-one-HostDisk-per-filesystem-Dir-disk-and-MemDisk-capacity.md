---
id: TASK-539.1
title: >-
  bucket_cell::host_disk: one HostDisk per filesystem; Dir::disk and MemDisk
  capacity
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 14:15'
updated_date: '2026-11-18 14:44'
labels:
  - extentd
  - disk
dependencies: []
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: medium
type: feature
ordinal: 933000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Refactor first. Both the stores and the part cache must charge one per-filesystem object keyed by st_dev. Tests must run a full disk through the production layers, which MemDisk cannot do today. (doc-22 task T1.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 crates/bucket_cell/src/host_disk.rs: HostDisk::of(path) returns the same Arc for two paths on one filesystem and different ones across filesystems (test with a tempdir and its parent, and /dev/shm on Linux).
- [ ] #2 HostDisk holds an open fd and samples it with fstatvfs (size, f_bavail·f_frsize, f_bfree). It has atomic counters stores, unframed, pledged, cache and cache_writing, plus the published allowance, headroom and pressure. Allowance defaults to u64::MAX.
- [ ] #3 The test free_bytes_are_the_free_blocks_size moves from cache.rs to host_disk.rs and passes.
- [ ] #4 Dir gains fn disk(&self) -> Option<&Arc<HostDisk>>. MemDisk::with_capacity(n) makes write_at, create and create_dir past n fail with ENOSPC, and its HostDisk reports n minus what it holds (tests assert the errno and the numbers).
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. bucket_cell::host_disk: HostDisk per filesystem (st_dev registry of Weak), fstatvfs sampler or in-memory sampler, Count counters (stores, unframed, pledged, cache, cache_writing), governed allowance/headroom/pressure, wake_governor/wait_for_governor.
2. The part cache samples free space through its HostDisk; the free-space test moves to host_disk.rs.
3. Dir::disk() (default None); DiskDir holds HostDisk::of(path); CachedDir forwards; MemDisk has a disk of its own, MemDisk/MemDir::with_capacity(n) refusing writes, files and directories past n with ENOSPC; its disk's sample says n minus what it holds (files counted in granules once per inode, directories a granule).
<!-- SECTION:PLAN:END -->
