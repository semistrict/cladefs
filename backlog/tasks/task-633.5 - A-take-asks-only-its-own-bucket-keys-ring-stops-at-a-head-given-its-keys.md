---
id: TASK-633.5
title: 'A take asks only its own bucket: keys::ring stops at a head given its keys'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 17:03'
updated_date: '2026-11-18 23:44'
labels:
  - bucket
  - multi-region
dependencies:
  - TASK-633.4
documentation:
  - >-
    backlog/docs/doc-23 -
    Cells-across-buckets-a-bucket-per-cloud-region-forks-that-span-them.md
parent_task_id: TASK-633
priority: high
type: feature
ordinal: 936000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
`keys::ring` runs `head::get(bucket, parent)` for every ancestor even when the child's head holds every key it inherited (`given`), and a writer's checkpoint follows inherited objects' moves. So a child cannot be taken or checkpoint while any ancestor's bucket is out of reach, though it holds all it needs (doc-23, A take walks every ancestor's head).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Taking a cell whose head is given reads no ancestor's head, and still refuses a head missing an inherited key
- [ ] #2 A child is taken, commits, uploads and checkpoints with its parent's bucket failing every request (test with a failing bucket)
- [ ] #3 A head made before its fork finished still walks, and its fork resumes when the parent's bucket answers
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18: keys::ring no longer reads any ancestor's head for a head given its keys: it checks its parent's keys are held (Error::Inherited otherwise) and returns; a head without keys still walks. Test: rings_cross_cells_until_heads_hold_what_they_inherit deletes the parent's head before the given child's ring; a hand mutant walking anyway fails with Lost. Left: a writer's checkpoint reading inherited objects' moves in their own bucket, which needs TASK-633.4's routing, and the failing-bucket test of AC #2.

2026-11-18, with TASK-633.4's routing in: a child's writer reads the moves of inherited objects through the router, so in the parent's bucket. AC #2's "checkpoints with its parent's bucket failing" does not hold before the version is mirrored: write_checkpoint follows moves under the parent's prefix (maps.read, maps.follow), and a child with no checkpoint of its own recovers through its origin's state. Taking, committing and uploading a child with a checkpoint of its own need only its bucket. doc-23's outage table corrected. The rest of AC #2 waits for TASK-633.8, whose mirror would route the parent's version's keys to the child's bucket.
<!-- SECTION:NOTES:END -->
