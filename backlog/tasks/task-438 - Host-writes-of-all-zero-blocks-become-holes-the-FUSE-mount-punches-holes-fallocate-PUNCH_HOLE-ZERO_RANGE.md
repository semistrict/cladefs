---
id: TASK-438
title: >-
  Host writes of all-zero blocks become holes; the FUSE mount punches holes
  (fallocate PUNCH_HOLE, ZERO_RANGE)
status: To Do
assignee: []
created_date: '2026-11-15 11:25'
labels:
  - fuse
  - snapshot
dependencies:
  - TASK-435
priority: medium
ordinal: 784000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A Firecracker full snapshot writes all of guest RAM (vendor/firecracker src/vmm/src/vstate/memory.rs, dump: write_all_volatile of every plugged slot) after set_len makes the file sparse, so the zeros of untouched memory and of pages the guest reports free through the balloon (CONFIG_PAGE_REPORTING=y; Firecracker drops reported ranges) are written, allocated by Fxfs and stored as extents. Nothing on the way drops them: the host FUSE server writes what it is given, and refuses fallocate modes but 0 (fuse/server.rs fallocate, ENOTSUP), though fxfs-posix has punch, which the guest's Fxfs uses. Found exploring a snapshot's memory file in its cell (TASK-435), 2026-11-15.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A write through the host's filesystem of block-aligned all-zero blocks leaves holes there, freeing what the file had, and takes no new extent, shown by a test that overwrites data with zeros and checks the file's blocks
- [ ] #2 The FUSE mount serves fallocate's FALLOC_FL_PUNCH_HOLE|KEEP_SIZE and FALLOC_FL_ZERO_RANGE through fxfs-posix's punch, shown by a test through the kernel
- [ ] #3 fxfs-guest-snapshot-memory-mounted-dax checks the memory volume holds fewer blocks than the VM's memory
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
