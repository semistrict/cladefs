---
id: TASK-711
title: >-
  fxfs-guest-pgbench-dax fails its encryption check on main: a cell's heads 8
  and 9 have no DEK
status: To Do
assignee: []
created_date: '2026-11-20 22:39'
labels:
  - bug
  - encryption
dependencies: []
priority: high
ordinal: 1113000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On ubu1, main at f247775cf (TASK-707's switch, which touches no encryption), fxfs-guest-pgbench-dax (CLADEFS_PGBENCH_RUNS=3, RUN_SECS=30) passed its pgbench runs and then failed the run's encryption check: 'not encrypted: 2 stores, 2 cells, 385 objects, 3467 files (3817 MiB) read: 2 in the clear' — tenants/0000000000000/cells/0008cmqw00000/head/0000000000008 and .../head/0000000000009: 'cell 0/8cmqw00000 wrote objects, its head has no DEK'. Every run of the case failed so, tracing on and CLADEFSD_TRACES=off alike. Logs: ubu1:/mnt/fxdax-scratch/pgb-ab/fxfs-guest-pgbench-dax/on-1/fxfs-guest-pgbench-dax/ (cladefsd.log, the bucket). Suspect TASK-690 (8b5722b41, f33db2873): acknowledgements now run beside uploads and one head swap names many segments; a head written that way may drop the DEK.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause is named, with the head writer's path that leaves the DEK out
- [ ] #2 fxfs-guest-pgbench-dax passes its encryption check on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
