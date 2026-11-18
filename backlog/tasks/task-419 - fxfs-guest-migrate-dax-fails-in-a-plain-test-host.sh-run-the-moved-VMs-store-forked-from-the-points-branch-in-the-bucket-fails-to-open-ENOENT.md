---
id: TASK-419
title: >-
  fxfs-guest-migrate-dax fails in a plain test-host.sh run: the moved VM's
  store, forked from the point's branch in the bucket, fails to open (ENOENT)
status: To Do
assignee: []
created_date: '2026-11-15 00:49'
updated_date: '2026-11-16 22:52'
labels:
  - bug
dependencies: []
priority: high
ordinal: 809000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-417's ubu1 run t417c (cladefs 51966aa12, fork e7c0a413e): CLADEFS_E2E_TEST=fxfs-guest-migrate-dax under scripts/test-host.sh, one host, no CLADEFS_MIGRATE_PHASE. The source VM snapshots and the restore (PUT /snapshot/load with cladefs_overrides naming elsewhere-moved/store-moved, a point_on_host that does not exist here, and the test daemon's socket) fails: "Cannot open the extent store .../elsewhere-moved/store-moved: ... ENOENT". unexplained-refusals.txt lists 8 refused stores to sealed pages of the source VM's window. The case usually runs under scripts/migrate-host.sh. Whether main fails too: baseline run t417d (main df59917ba) pending.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The cause is known: a regression of TASK-417's, or one on main, or the case not meant for a single-phase run
- [ ] #2 fxfs-guest-migrate-dax passes on ubu1 as scripts/migrate-host.sh runs it, and as test-host.sh does if it is meant to
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Baseline t417d on main df59917ba (fork 527247f75): fails the same, "Cannot open the extent store .../elsewhere-moved/store-moved: ... ENOENT". Not TASK-417's: on main before it.
<!-- SECTION:NOTES:END -->
