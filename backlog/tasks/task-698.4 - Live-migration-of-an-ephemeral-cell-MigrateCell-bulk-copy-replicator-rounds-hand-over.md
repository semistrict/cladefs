---
id: TASK-698.4
title: >-
  Live migration of an ephemeral cell: MigrateCell, bulk copy, replicator
  rounds, hand-over
status: To Do
assignee: []
created_date: '2026-11-20 16:21'
updated_date: '2026-11-20 16:22'
labels: []
dependencies:
  - TASK-698.1
  - TASK-698.2
  - TASK-698.3
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
parent_task_id: TASK-698
ordinal: 1093000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Sandboxes on ephemeral cells must move between hosts while running. The user chose pre-copy; guest RAM is the VM manager job. Sealed extents never change, so rounds converge, and doc-21 replicator already streams a cell log to a peer (doc-25, Live migration).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 MigrateCell on S writes T reference first, sends closed raw segments sparse, then streams rounds until the VM manager asks for the hand-over
- [ ] #2 At hand-over S commits, sends the rest and a local checkpoint, gives the cell up, and only then names its last commit; T opens only for that commit
- [ ] #3 Hand-overs failing before and after S gives up are tested: S stays writer, or T becomes it, never both
- [ ] #4 A guest case migrates a running VM disk between two daemons under writes; downtime and bytes sent are recorded on the test host
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
