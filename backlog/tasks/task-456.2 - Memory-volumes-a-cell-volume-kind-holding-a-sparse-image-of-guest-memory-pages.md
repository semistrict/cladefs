---
id: TASK-456.2
title: >-
  Memory volumes: a cell volume kind holding a sparse image of guest memory
  pages
status: To Do
assignee: []
created_date: '2026-11-15 13:33'
labels:
  - memory
  - host
dependencies:
  - TASK-456.1
parent_task_id: TASK-456
priority: high
ordinal: 863000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A third cell::Kind beside fxfs and sqlite (extent_remote/src/cell.rs): a store whose root names one page tree, the image of a VM's memory, no filesystem. Read by page; written by page or as holes, a batch committed at once; forked as any branch, so a restored VM's next snapshot is a fork of its parent's memory with only what changed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A memory volume is made in a cell, listed in the cell's object with kind memory, opened through extentd by the tenant whose cell it is and no other, shown by tests
- [ ] #2 A batch of pages and holes is committed in one commit; a crash before it leaves the image as the last commit had it, shown by a crash test
- [ ] #3 A fork of a memory volume reads the parent's pages until it writes its own, and stores only what it wrote, shown by a test counting the fork's own extents
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
