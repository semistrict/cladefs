---
id: TASK-508.3
title: 'One concept, one type: a cell'
status: To Do
assignee: []
created_date: '2026-11-18 00:10'
updated_date: '2026-11-18 02:23'
labels:
  - bucket
dependencies:
  - TASK-508.2
documentation:
  - doc-20
parent_task_id: TASK-508
ordinal: 900000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Today two types name what is one thing, a cell, of one kind, with a root and a capacity (doc-20): Cell (the cell object: holder, epoch, token, KEKs, shredded, its branch and kind) and Branch (its log, whose head is a second swapped object) fold into one, Cell, with one swapped object.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A cell has one swapped object
- [ ] #2 One type names a cell
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18: not started. One swapped object per cell needs points gone first: a point is a branch of its own in a cell with no object (keys.rs: 'a point's cell, which no host makes'), so while points stand, cells still hold branches the cell object doesn't name, and the head (Head::versions) is also the ground TASK-476 is changing. Order: TASK-476's daemon versions, TASK-508.2, then this, with specs/bucket's cell and placement models.
<!-- SECTION:NOTES:END -->
