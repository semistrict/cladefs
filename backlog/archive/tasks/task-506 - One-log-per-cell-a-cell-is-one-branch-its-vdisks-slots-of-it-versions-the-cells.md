---
id: TASK-506
title: >-
  One log per cell: a cell is one branch, its vdisks slots of it, versions the
  cell's
status: To Do
assignee: []
created_date: '2026-11-17 02:46'
updated_date: '2026-11-18 00:11'
labels:
  - bucket
  - host
  - store
dependencies: []
documentation:
  - doc-19
priority: high
ordinal: 892000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user (2026-11-17) wants a cell to be one distributed object with one writer, holding every vdisk in it and every version of it, so that a version of the whole cell is extremely cheap: one commit, one file. Today each vdisk is its own branch with its own log and head, so a whole-cell version needs N pauses, N segment-closing commits, N uploads and N head swaps (doc-18). TASK-364 first chose one log per cell and turned from it because the store assumed one client per branch; doc-19 answers each assumption. Supersedes TASK-364.1's pause-every-vdisk fork and doc-18's whole-cell section.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A store holds several vdisks, each committing on its own: a commit seals only its vdisk's open extents and records only its frees, root and capacity
- [ ] #2 A version is a whole cell's: one commit of the cell's branch, named by its head; cell_cut is gone
- [ ] #3 A cell's vdisks share one branch, one head and one store in the daemon; the cell object names its branch, not a vdisk map
- [ ] #4 A fork of a cell carries every vdisk, each with a namespace of its own
- [ ] #5 doc-7, doc-14, doc-18 and CONTEXT.md describe one log per cell
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
