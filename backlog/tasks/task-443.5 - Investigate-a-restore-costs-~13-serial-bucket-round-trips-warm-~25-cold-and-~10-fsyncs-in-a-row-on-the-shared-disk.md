---
id: TASK-443.5
title: >-
  Investigate: a restore costs ~13 serial bucket round trips warm (~25 cold) and
  ~10 fsyncs in a row on the shared disk
status: To Do
assignee: []
created_date: '2026-11-15 12:24'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: high
ordinal: 839000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fork of a point then open of the child: branch::fork_point/finish_point_fork, upload.rs child_free and Shared::start, recovery::recover_layers/state_at, cell::hold, head::take_from, Writer::take, each awaited in turn; the child's record is read three times and its head twice for a branch made milliseconds before. Locally store_dir::write/inherit, Store::make, made_host and Segments::open each sync a file and its directory. Estimated 0.4-0.75 s of bucket time at 30 ms a request and seconds on a busy disk (TASK-333 measured 0.1-1.4 s a sync there). Each fork step also builds a throwaway tokio runtime (upload.rs block_on), so bucket connections it opens may die with it. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The ignored remote::tests::bucket_round_trips_by_phase gains a point-fork and a point-fork-open phase and its counts are recorded; fsync-class calls and their total time around one restore are counted beside 8 VMs writing to the same disk
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
