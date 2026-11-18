---
id: TASK-506.2
title: The daemon opens a cell's store once; its vdisks' connections share it
status: To Do
assignee: []
created_date: '2026-11-17 02:47'
updated_date: '2026-11-18 00:11'
labels:
  - host
dependencies: []
parent_task_id: TASK-506
ordinal: 894000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-19 stage 3. Today one connection opens one store, one vdisk.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 OPEN of a vdisk of a cell already open shares the cell's store, holding its slot
- [ ] #2 The FUSE mount's vdisks of a cell are slots of one store
- [ ] #3 A VM's disk and memory vdisks are slots of one store
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Superseded 2026-11-17 by TASK-508 (doc-20, a cell is one filesystem). Built on t3/one-log-per-cell, kept unmerged as a reference.
<!-- SECTION:NOTES:END -->
