---
id: TASK-443.6
title: >-
  Investigate: restores of a point still being opened fail EBUSY after 10 s
  instead of waiting
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
ordinal: 840000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/fxdax/src/extentd/stores.rs: the first restore of a point holds its key through take_store, a full open; every other restore waits in hold_to_fork with CLOSING_WAIT (10 s) and returns EBUSY; fork_from retries only Opened::Before. 200 restores of one snapshot on a cold host with a slow bucket or a saturated disk would fail in a burst. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 20 concurrent ForkFrom opens of one cold point with the bucket slowed so its open exceeds 10 s: the count of EBUSY is recorded
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
