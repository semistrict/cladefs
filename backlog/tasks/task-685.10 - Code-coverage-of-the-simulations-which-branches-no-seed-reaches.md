---
id: TASK-685.10
title: 'Code coverage of the simulations: which branches no seed reaches'
status: To Do
assignee: []
created_date: '2026-11-20 07:45'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1079000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user, 2026-11-20: examine the code coverage the simulation achieves, to see the branches it does not cover. Branch coverage (cargo-llvm-cov, Apache-2.0/MIT, over rustc's -C instrument-coverage) of the simulation runs alone: local_cell::sim's profiles now (a baseline), the full-stack madsim simulation as its steps land. A report of the protocol code's branches no seed reaches (upload.rs, replicate.rs, cell.rs, bucket_cell's writer and head, cladefsd's cells/server once simulated), each either reached by a new guest action or fault profile, or argued unreachable, or left to a named test.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A script runs the simulations under coverage and writes a branch report of the protocol crates, alone (not the unit tests)
- [ ] #2 A baseline report for local_cell::sim's profiles, its uncovered branches in upload.rs, replicate.rs and cell.rs listed with what reaches each, or why none does
- [ ] #3 The full-stack simulation's coverage reported the same way once it runs (TASK-685.6), in CI beside its seeds (TASK-685.8)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
