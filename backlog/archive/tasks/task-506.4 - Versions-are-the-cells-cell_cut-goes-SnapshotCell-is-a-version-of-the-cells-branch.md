---
id: TASK-506.4
title: >-
  Versions are the cell's: cell_cut goes, SnapshotCell is a version of the
  cell's branch
status: To Do
assignee: []
created_date: '2026-11-17 02:47'
updated_date: '2026-11-18 00:11'
labels:
  - bucket
  - host
  - snapshot
dependencies: []
parent_task_id: TASK-506
ordinal: 896000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-19 stage 5, with doc-18 rewritten.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 VersionCut and Head::versions carry no cell_cut
- [ ] #2 SnapshotCell takes one version of the cell's branch, every vdisk quiescent
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
