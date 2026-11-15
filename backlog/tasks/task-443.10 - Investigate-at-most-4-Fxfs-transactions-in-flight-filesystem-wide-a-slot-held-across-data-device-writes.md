---
id: TASK-443.10
title: >-
  Investigate: at most 4 Fxfs transactions in flight filesystem-wide, a slot
  held across data device writes
status: To Do
assignee: []
created_date: '2026-11-15 12:25'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: medium
ordinal: 844000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/fxfs/src/filesystem.rs MAX_IN_FLIGHT_TRANSACTIONS is 4; a slot is held from Transaction::new to drop, across the device write in multi_write_internal, the wait for the commit mutex and journal space; each release wakes every waiter. It would explain TASK-288's profile (21% in fxfs_rt_park under create; fsync flat at 4-5k IOPS for any job count), which does not name it. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 scale-create-j8 and create-fsync at 8 jobs are compared with the limit at 4 and at 64
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
