---
id: TASK-456.2
title: >-
  Memory volumes: a cell volume kind holding a sparse image of guest memory
  pages
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 13:33'
updated_date: '2026-11-15 20:13'
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

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extent_files: holes written: a file's dirty chunks are data or holes (punch of whole chunks); a commit places a hole as no ref, leaving the block it replaces; reads of it zeros.
2. extent_remote::cell::Kind::Memory ('memory'); extentd: wire OPEN_MEMORY beside OPEN_SQLITE (wire version bump), codec, control API VolumeKind::MEMORY, client open_memory; Go bindings regenerated.
3. cladefs::memory_volume: a memory volume opened through the daemon (as sqlite_extents::daemon opens SQLite's): an extent_files volume of one file, 'memory', of the guest memory's size; write pages and holes, commit them at once, read pages, the runs of pages a window maps.
4. Tests: kind round trip and cell listing; tenant isolation (another tenant's open fails EACCES, as stores'); a batch of pages and holes committed once and a crash test with FakeDevice crashes (the image is the last commit's); a fork reads its parent's pages and stores only what it wrote (its own extents counted).
<!-- SECTION:PLAN:END -->
