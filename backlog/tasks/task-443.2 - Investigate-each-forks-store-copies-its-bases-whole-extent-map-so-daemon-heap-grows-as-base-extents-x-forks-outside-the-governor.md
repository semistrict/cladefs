---
id: TASK-443.2
title: >-
  Investigate: each fork's store copies its base's whole extent map, so daemon
  heap grows as base extents x forks, outside the governor
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
ordinal: 836000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/extent_store/src/store.rs Recovering::open collects every extent of the branch, inherited ones too, into a HashMap<ExtentId, Extent> (layout.rs), each with an owned key String (extent_frame::segment::Location). Estimated 150-200 B an extent a fork: ~2 GB at 32k extents and 400 forks, ~20 GB at 300k. It is anonymous memory outside the governor and the anon budget, so the failure is the daemon OOM-killed with every VM. TASK-92.55 slice 4 and TASK-242 name the copy for open time, not memory; TASK-34's 91 KiB a fork was at 64 extents. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 extentd::tests::serves_hundreds_of_forks_of_a_base (or its like) run with a base of 32,768 and of 300,000 extents: daemon anonymous memory per open idle fork, and the extent count of a real sandbox base image, are recorded
- [ ] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
