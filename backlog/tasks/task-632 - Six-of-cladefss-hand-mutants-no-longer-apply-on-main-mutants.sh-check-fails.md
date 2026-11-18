---
id: TASK-632
title: >-
  Six of cladefs's hand mutants no longer apply on main: mutants.sh --check
  fails
status: To Do
assignee: []
created_date: '2026-11-18 16:45'
labels:
  - testing
dependencies: []
priority: high
type: bug
ordinal: 948000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/cladefs/mutants.sh --check, which scripts/test.sh's scripts part runs (CI's clippy scripts job), fails at a1167a56a with 6 of 22 broken: a-second-open-holds-its-path and tenants-name-branches (cladefsd/server.rs), copies-of-whole-requests (src/extents.rs, gone), forks-take-branches-claimed (host_store.rs and bucket_cell/src/branch.rs, gone since TASK-508's renames), framed-in-copies and large-extents-framed-in-copies (local_cell/src/store.rs, gone: the store is cell.rs). Found while retargeting the disk mutants for TASK-539. Each is to be retargeted at the code that now does what it broke, and still killed by its test, or, if its subject is gone, said so and replaced.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 crates/cladefs/mutants.sh --check reports no patch broken
- [ ] #2 Each retargeted mutant is killed by the test its header names (mutants.sh NAME on ubu1)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
