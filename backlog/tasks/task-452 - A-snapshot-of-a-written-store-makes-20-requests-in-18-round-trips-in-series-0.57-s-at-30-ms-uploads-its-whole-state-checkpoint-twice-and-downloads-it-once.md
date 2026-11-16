---
id: TASK-452
title: >-
  A snapshot of a written store makes 20 requests in 18 round trips in series
  (0.57 s at 30 ms), uploads its whole-state checkpoint twice and downloads it
  once
status: To Do
assignee: []
created_date: '2026-11-15 12:58'
labels:
  - perf
  - review-2026-11-15
dependencies: []
references:
  - TASK-443.5
  - TASK-443.14
documentation:
  - docs/measurements/macos-bucket-lifecycle-2026-11-15/note.md
priority: medium
type: enhancement
ordinal: 852000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Measured by TASK-443.14 (bucket_lifecycle_by_phase; docs/measurements/macos-bucket-lifecycle-2026-11-15). A point of a fork that wrote one block since its last checkpoint: 20 requests, at most 2 in flight, 574 ms of bucket time at 30 ms a request, 1,835 ms at 100 ms, with the VM paused in production. It creates the checkpoint at the fork point (recovery::checkpoint_at, from branch::record_fork_under) after reading its own head twice, listing its checkpoints twice and reading its segments' tails; then writer.checkpoint() lists the moves again and creates the same key again with the same state, which the bucket refuses, the body sent, and bucket::create then GETs the object back to compare: 215.8 KB up and 108.7 KB down at 32,768 extents, 1,714 KB up and 858 KB down at 262,144, for 584 bytes written. The child's record is read 4 times. Not measured here: the wait for the backlog's upload with the VM paused (TASK-443.14, Linux).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A snapshot uploads its checkpoint once and downloads none of its own state, shown by bucket_lifecycle_by_phase
- [ ] #2 A snapshot makes at most half the round trips in series it does today (18), pinned by a test counting its requests
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
