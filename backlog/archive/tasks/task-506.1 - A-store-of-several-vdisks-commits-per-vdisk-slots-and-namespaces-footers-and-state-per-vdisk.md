---
id: TASK-506.1
title: >-
  A store of several vdisks: commits per vdisk, slots and namespaces, footers
  and state per vdisk
status: To Do
assignee: []
created_date: '2026-11-17 02:46'
updated_date: '2026-11-18 00:11'
labels:
  - store
  - bucket
dependencies: []
parent_task_id: TASK-506
ordinal: 893000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-19 stages 1 and 2: the store core and formats. cladefs keeps using slot 0 so that its tests pass unchanged.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Contents and the branch's State hold vdisks by slot (name, kind, root, next sequence, capacity) and spent per slot; own extents by full ID, keyed by branch
- [ ] #2 Store commits, frees, roots, capacities and next sequence numbers are per vdisk; a commit of one vdisk leaves another's open extents and frees uncommitted
- [ ] #3 A segment closes only when no open extent of any vdisk has pieces in it
- [ ] #4 Branch IDs from the allocator are aligned to VDISK_SLOTS; a vdisk past slot 0 is refused on an unaligned branch
- [ ] #5 Tests: two vdisks writing and committing interleaved, a crash between, recovery and a fork keep each vdisk's state; the existing suites pass
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
