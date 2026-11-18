---
id: TASK-508.1
title: 'A cell has one filesystem: the cell object''s vdisk map and cell cuts go'
status: In Progress
assignee: []
created_date: '2026-11-18 00:10'
updated_date: '2026-11-18 00:11'
labels:
  - bucket
  - host
dependencies: []
documentation:
  - doc-20
parent_task_id: TASK-508
ordinal: 898000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main's cells (TASK-364) hold a map of named vdisks, each a branch of its own, and versions carry a cell_cut for whole-cell cuts (doc-18). With a cell being one filesystem (doc-20) the map, add/remove vdisk, the vdisk name rules and cell_cut have nothing to do; a cell has a kind instead. A VM's disk and memory become cells of their own, and the daemon's API, the FUSE mount and the Go bindings stop naming vdisks.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cell object names no vdisks; a cell's kind is the cell's
- [ ] #2 NewVersion and Version carry no cell_cut; VersionCut is NewVersion
- [ ] #3 The daemon, the FUSE mount and the Go bindings make and open cells, not vdisks of cells
- [ ] #4 Every test suite that passed before passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
