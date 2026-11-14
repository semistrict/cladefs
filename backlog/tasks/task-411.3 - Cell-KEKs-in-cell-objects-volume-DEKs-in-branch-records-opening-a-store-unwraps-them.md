---
id: TASK-411.3
title: >-
  Cell KEKs in cell objects, volume DEKs in branch records; opening a store
  unwraps them
status: To Do
assignee: []
created_date: '2026-11-14 18:34'
updated_date: '2026-11-14 18:35'
labels: []
dependencies:
  - TASK-411.1
  - TASK-411.2
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 800000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-16, Keys: the tenant KEK wraps a cell KEK kept in <C>cell, which wraps each volume's DEK kept in its record, with generations. Record is Copy today and gains variable-length fields; layout FORMAT changes.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A new cell gets a cell KEK and a new branch a DEK, each stored only wrapped
- [ ] #2 Opening a store unwraps with one KMS call per cell taking, not per volume: a test counts calls
- [ ] #3 A store whose tenant KEK is revoked fails to open with an error naming the tenant
- [ ] #4 Rotating a cell KEK rewraps its volumes' DEKs by conditional updates; adding a DEK generation makes new objects use it while old ones stay readable
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
