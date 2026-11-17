---
id: TASK-443.12
title: >-
  Investigate: DAX has no fault-around (a fault per 4 KiB of every exec'd
  binary), and a full window reclaims hot mappings
status: To Do
assignee: []
created_date: '2026-11-15 12:25'
updated_date: '2026-11-17 01:10'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 846000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
guest/linux/fs/cladefs/dax.c fxfs_dax_vm_ops has no .map_pages, so each touched page of libc, ld.so and small files goes through fxfs_dax_do_fault, taking the inode's invalidate_lock and file sem and, with more than one hot range, the filesystem-wide dax->lock; fork children refault. Once the window fills, fxfs_dax_reclaim picks by an LRU only iomaps refresh, so a mapped library with live PTEs ages and is unmapped (an rmap walk per entry, a blocking UNMAP, every vCPU flushed), and faults wait in 10 ms steps. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Faults and time per exec of /bin/true and of a compiler on a warm window are recorded with DAX and without; and window_full, window_reclaims and window_unmaps_hist with command latency after reading twice a 512 MiB window
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
