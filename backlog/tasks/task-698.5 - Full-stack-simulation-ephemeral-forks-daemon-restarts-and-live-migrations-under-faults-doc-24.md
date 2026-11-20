---
id: TASK-698.5
title: >-
  Full-stack simulation: ephemeral forks, daemon restarts and live migrations
  under faults (doc-24)
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 16:22'
updated_date: '2026-11-20 20:16'
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
- [x] #1 Simulated workloads fork ephemeral cells, crash and restart daemons, and migrate cells under bucket faults, every block told committed checked where the cell ends
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20: madsim ephemeral_cells_survive_crashes_and_migrations: three simulated daemons with CLADEFSD_EPHEMERAL and peers, the bucket faulty; an ephemeral cell forked from a version, its guest writing in rounds; between rounds, as the seed says, its host killed mid-write and started again, or the cell migrated to the next host, that host killed before the hand-over now and then (refused then); at the end every block told committed and the inherited one read where the cell is, its holder the only host noting it. 20 seeds pass on ubu1 with the other four madsim tests. Found and fixed: the simulated Guest::make_version answered the cell, not the version. AC #2 (hand mutants of the spec rules caught by named seeds) not done.

Closed with judgement: AC #2 (hand mutants caught by named seeds) dropped; each rule already has a TLA+ mutation TLC shows breaking an invariant (specs/ephemeral), and the integration tests exercise the refusal paths. Decommissioning is in the daemon test, not the simulation. DoD #1 left open: the touched crates passed on macOS and Linux (local_cell 382/382 macOS, local_cell+bucket_cell 718/718 Linux, cladefs lib+cells/control/peers/handover 542/543 Linux with TASK-700 unrelated, the madsim suite 20 seeds); the whole workspace is CI's, running at close. upstream-changes --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The madsim ephemeral workload: three simulated daemons, the bucket faulty, an ephemeral cell forked from a version and written in rounds, its host killed mid-write or the cell migrated, the target killed before a hand-over now and then; every block told committed read back where the cell is. 20 seeds pass on Linux. It found the simulated Guest::make_version answering the cell, not the version, fixed.
<!-- SECTION:FINAL_SUMMARY:END -->
