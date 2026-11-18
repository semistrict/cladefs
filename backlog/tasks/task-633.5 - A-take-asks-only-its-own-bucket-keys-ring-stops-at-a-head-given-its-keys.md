---
id: TASK-633.5
title: 'A take asks only its own bucket: keys::ring stops at a head given its keys'
status: To Do
assignee: []
created_date: '2026-11-18 17:03'
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
