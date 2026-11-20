---
id: TASK-698.3
title: >-
  Ephemeral forks: ForkCell ephemeral, host incarnation, references by
  incarnation, retiring on start, DecommissionHost
status: To Do
assignee: []
created_date: '2026-11-20 16:21'
updated_date: '2026-11-20 16:22'
labels: []
dependencies:
  - TASK-698.1
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
parent_task_id: TASK-698
ordinal: 1092000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Forking a version into an ephemeral cell writes only a reference in the parent bucket, which must keep the version while the cell lives and be retired once it does not, including when the host never comes back (doc-25, Forking one). The user chose explicit decommissioning over leases.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 ForkCell with ephemeral set makes a cell whose ID comes from the bucket allocator, writing the reference before reading the parent head
- [ ] #2 The collector keeps a version while an ephemeral reference stands, and recognises the key kind as one with no head
- [ ] #3 A daemon starting retires its incarnation references whose cell it does not hold; DecommissionHost retires all of an incarnation
- [ ] #4 Ephemeral cells refuse replicas, FSYNC_LEVEL_REPLICATED and MakeVersion; their sync and syncfs wait for Local only
- [ ] #5 The proto, Go bindings and their docs describe the new fields and calls
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
