---
id: TASK-698.5
title: >-
  Full-stack simulation: ephemeral forks, daemon restarts and live migrations
  under faults (doc-24)
status: To Do
assignee: []
created_date: '2026-11-20 16:22'
updated_date: '2026-11-20 16:22'
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
- [ ] #1 Simulated workloads fork ephemeral cells, restart and crash daemons, decommission hosts and migrate cells, with the durability checker covering ephemeral cells
- [ ] #2 Hand mutants of the spec rules (reference written after the head read; T opening before S gives up) are caught by named seeds
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
