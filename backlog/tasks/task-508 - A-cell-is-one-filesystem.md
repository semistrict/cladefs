---
id: TASK-508
title: A cell is one filesystem
status: In Progress
assignee: []
created_date: '2026-11-18 00:10'
updated_date: '2026-11-18 00:11'
labels:
  - bucket
  - host
  - store
dependencies: []
documentation:
  - doc-20
priority: high
ordinal: 897000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user (2026-11-17) chose to have no vdisks: a cell is one filesystem with one writer, its log, head, key and versions, and a VM is several cells. Cells of several vdisks needed either N-way coordination for a whole-cell version (doc-18, cell cuts) or one log for all of them (doc-19), which moved the vdisks into the store's core and grew the code by about 3,700 lines on t3/one-log-per-cell. A paused VM needs neither: its VM snapshot makes a local version in each of its cells and writes a record naming them. Supersedes TASK-506 and doc-19.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A cell has one filesystem: no vdisk map, no cell cuts
- [ ] #2 Making a version is one local commit closing the segment; nothing on that path touches the bucket
- [ ] #3 Forks are made only from versions; points are gone
- [ ] #4 The words vdisk, branch, point, slot, namespace and cut are gone from code, docs and glossary, but for the exceptions doc-20 names
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
