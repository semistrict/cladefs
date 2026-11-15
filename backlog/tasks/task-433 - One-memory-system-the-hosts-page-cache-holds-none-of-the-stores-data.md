---
id: TASK-433
title: 'One memory system: the host''s page cache holds none of the stores'' data'
status: In Progress
assignee: []
created_date: '2026-11-15 10:26'
updated_date: '2026-11-15 14:02'
labels:
  - memory
dependencies: []
priority: high
ordinal: 823000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
cladefs owns caching (its extent cache, the governor's budget, held parts, fetch budgets); the OS page cache is a second, unbudgeted memory system holding the same bytes. User, 2026-11-15: none of the uses have good reasons; all data I/O bypasses the page cache, our own memory budgeting holds what is held. FUSE mounts' FOPEN_KEEP_CACHE stays (the kernel's FUSE client is the consumer).
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
433.2, 433.4 done. 433.1, 433.3 need a write-behind of our own first: the page cache is what lets a guest WRITE and a framing return before the disk (0.5 ms a WRITE on a network disk, HostDir's docs; TASK-401).

2026-11-15, with the user: 433.1 and 433.3 by a general page cache of fxdax's own, replacing the OS page cache for the store's data files (write-behind batching into large aligned O_DIRECT writes, reads of pending bytes from memory, fsync = flush then sync, eviction under the governor's one budget), designed after Linux's (mm/filemap.c, mm/page-writeback.c, fs/fs-writeback.c; ~/src/linux, 7.3-rc3). Then AGENTS.md: never rely on the OS page cache for substantial data (two memory budgets).

433.1, 433.3 done (d2cfd3b4c); AGENTS.md rule added. 433.5 left.
<!-- SECTION:NOTES:END -->
