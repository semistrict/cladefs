---
id: TASK-395.19
title: A pass lists a branch's checkpoints once more for each point it was forked at
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 15:48'
updated_date: '2026-11-14 17:00'
labels:
  - collector
  - performance
dependencies: []
parent_task_id: TASK-395
priority: low
ordinal: 797000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found measuring TASK-395.12: a pass over a cell whose branch was forked at N points to other cells makes N+1 LISTs of the branch's checkpoints (fxdax-gc --cell over fxdax-gc-fill --vm 1024 100: list 104 against 5 with one point). TASK-395.16's Checkpoints::listed serves the first listing of each branch from the pass's listing of the cell, once a branch: each fork point's recovery (recovery::state_at, by recover_at) lists anew. A state as of a segment is the same from any checkpoint up to there, so the listing serves each fork point's first as well as a branch's; a restart lists anew. On S3 a LIST costs 12.5 GETs.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A pass over a cell whose branch was forked at many points lists its checkpoints as many times as with one, shown by a request-counting test that goes red with the change undone
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Checkpoints::served keyed by (branch, fork point or none); state_at's kept flag: recover_at's fork points get their own first listing from the cell's listing; restarts list anew; in-cell origins keep the branch's.
2. collector test: checkpoints listed once for 1 and 6 fork points; red with kept false.
3. doc-7's LIST accounting.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Found measuring TASK-395.12 (list 104 for 100 fork points, 5 for one). Safe by TASK-395.16's argument: a state as of a segment is the same from any checkpoint up to there; one found gone, or a segment missing, starts the recovery over with a fresh listing; what the pass deletes it listed, so a checkpoint made since the listing stays. Test passes_list_a_branchs_checkpoints_once_however_many_points_it_was_forked_at: (1, [1]) and (1, [1;6]); with state_at listing under (branch, None) it fails (2, [1]). Spec unchanged, as TASK-395.16's (Bucket.tla's pass reads each fork point's state as of then). Blind spot: TASK-395.16's test pinned LISTs only for cells without fork points; this one adds them. 100-point pass: list 104 -> 4.

First cut served every state_at by (branch, at), in-cell origins too: children_restored_from_points_keeping_their_origins_need_recoveries_by_origins then passed with its rule off, as the origin's recovery read the cell's listing (no checkpoint yet) and kept segments 0..3 rather than reading the checkpoint made mid-pass. Safe (keeps more) but the mutation test lost its scenario, so only kept fork points (recover_at) get their own listing. extent_remote 307/307 after.
<!-- SECTION:NOTES:END -->
