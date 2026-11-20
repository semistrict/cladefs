---
id: TASK-709
title: >-
  hostile: a_compromised_vm_process_harms_no_tenant_beside_it fails 3 in 8 on
  ubu1, share_version ENOENT on a cell whose connection just dropped
status: To Do
assignee: []
created_date: '2026-11-20 22:21'
labels:
  - testing
  - bug
dependencies: []
priority: medium
type: bug
ordinal: 1113000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen in TASK-702.1's Linux run, then reproduced on main at c1ac6d06 (8 runs on ubu1 under fxjob: 3 FAIL in ~0.4 s, 5 PASS in 16-21 s), so it predates TASK-702. crates/cladefs/src/cladefsd/tests/hostile.rs:1845: `control(super::super::cells::share_version(dir.path().join("given"), ...)).unwrap()` fails Err(Errno(ENOENT)) right after `drop(given)`, the CellClient that made the version: the store is likely still closing (its connection is torn down asynchronously) when share_version opens it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The test passes 50 runs in a row on ubu1, with the cause of the ENOENT named in this task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
