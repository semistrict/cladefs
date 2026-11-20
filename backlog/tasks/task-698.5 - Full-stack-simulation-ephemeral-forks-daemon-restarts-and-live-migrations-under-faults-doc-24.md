---
id: TASK-698.5
title: >-
  Full-stack simulation: ephemeral forks, daemon restarts and live migrations
  under faults (doc-24)
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 16:22'
updated_date: '2026-11-20 20:08'
labels: []
dependencies:
  - TASK-698.4
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
parent_task_id: TASK-698
ordinal: 1094000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The protocols of doc-25 span daemons, peers and the bucket, which is what the doc-24 simulation exists to break under one seed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Simulated workloads fork ephemeral cells, restart and crash daemons, decommission hosts and migrate cells, with the durability checker covering ephemeral cells
- [ ] #2 Hand mutants of the spec rules (reference written after the head read; T opening before S gives up) are caught by named seeds
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20: madsim ephemeral_cells_survive_crashes_and_migrations: three simulated daemons with CLADEFSD_EPHEMERAL and peers, the bucket faulty; an ephemeral cell forked from a version, its guest writing in rounds; between rounds, as the seed says, its host killed mid-write and started again, or the cell migrated to the next host, that host killed before the hand-over now and then (refused then); at the end every block told committed and the inherited one read where the cell is, its holder the only host noting it. 20 seeds pass on ubu1 with the other four madsim tests. Found and fixed: the simulated Guest::make_version answered the cell, not the version. AC #2 (hand mutants of the spec rules caught by named seeds) not done.
<!-- SECTION:NOTES:END -->
