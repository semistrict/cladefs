---
id: TASK-407.6
title: >-
  A fork point's checkpoint can be written again after its reference is retired:
  whoever records a fork across cells writes it at one key
status: To Do
assignee: []
created_date: '2026-11-15 02:25'
updated_date: '2026-11-15 10:18'
labels: []
dependencies: []
parent_task_id: TASK-407
priority: low
type: bug
ordinal: 803000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
recovery::checkpoint_at (TASK-395.4) writes the parent's state at a cross-cell fork point, create-only, at the parent's checkpoint key for that segment, whoever records the fork: its writer, the next, or the parent's deletion, each computing it from the newest checkpoint up to there, so their bytes may differ (moves followed). Once the child is deleted and its reference retired, the parent's cell's pass deletes it; a stale recorder that read no child record before can then create it again, other bytes at a key once read (TASK-407's rule: a key is written once). No reader reads it again (the child, the only one, is deleted, and IDs are never reused), so it is garbage, not a wrong read; but caches may hold the old object's parts under the key. Found designing TASK-407.2 (2026-11-14); specs/bucket does not model fork points' checkpoints (StateAt there lists). Fix: key it by its writer's taking (unique holder) and have the child's record name it, or make its bytes a function of the fork point alone.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A fork point's checkpoint key is written by one process, once; the child's recovery reads the one its record names
- [ ] #2 A test or the spec shows a stale recorder writing after the retirement writes another key
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15, decided with the user: left filed. The re-created checkpoint is never read (a deleted child's ID is never reused), so no cache serves anything stale from it, and a later pass deletes it. Keying it by its recorder would have the parent's cell's passes read each child's record across cells to know which to keep (TASK-395.4 avoids that); identical bytes from any recorder would need extents named where sealed, recovery following moves from there. Neither is worth it for garbage.
<!-- SECTION:NOTES:END -->
