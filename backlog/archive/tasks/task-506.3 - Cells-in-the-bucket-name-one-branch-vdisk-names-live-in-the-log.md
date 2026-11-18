---
id: TASK-506.3
title: Cells in the bucket name one branch; vdisk names live in the log
status: To Do
assignee: []
created_date: '2026-11-17 02:47'
updated_date: '2026-11-18 00:11'
labels:
  - bucket
  - host
dependencies: []
parent_task_id: TASK-506
ordinal: 895000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-19 stage 4. cell::Object's vdisks map goes; the cell is taken with its one head.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cell::Object names its branch; no vdisk map
- [ ] #2 CreateVdisk and DeleteVdisk are commits of the cell's branch
- [ ] #3 specs/bucket and the simulation hold
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
