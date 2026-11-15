---
id: TASK-423
title: >-
  extent_store's extents_written_in_runs_seal_where_they_are flakes since
  TASK-400: the commit's I/O window counts 1 MiB read, the look-ahead packing a
  part beside it
status: To Do
assignee: []
created_date: '2026-11-15 05:06'
labels:
  - testing
  - flaky
  - bug
dependencies: []
priority: high
ordinal: 817000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main's CI test (workspace) on 7998d1f86 (run 43675449547): crates/extent_store/tests/store.rs extents_written_in_runs_seal_where_they_are, assert_eq!((sealed.read - before.read, sealed.written - before.written), (0, 0)) failed with left (1048576, 0). The test measures MemDir's I/O across store.commit_ordered to prove the seal reads and writes nothing; since c2daf7630 (TASK-400) a segment's parts are read and packed ahead on idle-priority threads as they are written whole, asynchronously, so a 1 MiB part read by the look-ahead can land inside the window. Either the test must not count the look-ahead's reads (wait for it to settle before measuring, or measure only the commit's own I/O), or the look-ahead must not read what the commit is sealing; decide which the code promises. Not the seal reading back (TASK-92.1).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The test passes deterministically: 0 failures over many runs under load, and a run that forces the look-ahead into the window fails without the fix
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
