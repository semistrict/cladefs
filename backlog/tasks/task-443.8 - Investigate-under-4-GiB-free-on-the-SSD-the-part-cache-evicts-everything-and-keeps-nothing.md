---
id: TASK-443.8
title: >-
  Investigate: under 4 GiB free on the SSD the part cache evicts everything and
  keeps nothing
status: To Do
assignee: []
created_date: '2026-11-15 12:25'
updated_date: '2026-11-16 22:53'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 842000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/extent_remote/src/cache.rs Kept::make_room evicts until free + freed >= floor + needed, with the floor CACHE_FLOOR (4 GiB, crates/cladefs/src/cladefsd/stores.rs) and free from statvfs of the disk the stores' WALs share; when WALs back up behind a slow bucket it walks by_use to its end and the new part is still not kept, so every read becomes a bucket GET exactly when the bucket is slow. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cladefsd_part_cache_bytes and GETs are recorded while the cache's disk is filled to under 4 GiB free
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
