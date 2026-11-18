---
id: TASK-508.3
title: 'One concept, one type: a cell'
status: To Do
assignee: []
created_date: '2026-11-18 00:10'
updated_date: '2026-11-18 00:24'
labels:
  - bucket
dependencies: []
documentation:
  - doc-20
parent_task_id: TASK-508
ordinal: 900000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Today three types name what is one thing, a cell: one filesystem with a kind, a root and a capacity, on one log (doc-20). Cell (the cell object: holder, epoch, token, KEKs, shredded), Branch (its log, whose head is a second swapped object) and cell::Filesystem (its kind and log) fold into one, Cell, with one swapped object.
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
