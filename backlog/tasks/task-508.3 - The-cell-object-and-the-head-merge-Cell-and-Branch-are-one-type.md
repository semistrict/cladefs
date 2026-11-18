---
id: TASK-508.3
title: The cell object and the head merge; Cell and Branch are one type
status: To Do
assignee: []
created_date: '2026-11-18 00:10'
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
With one filesystem per cell, a cell has exactly one head, so its object (holder, epoch, token, KEKs, shredded) and its head are two swapped objects for one thing, and Cell and Branch are two types for one name (doc-20).
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
