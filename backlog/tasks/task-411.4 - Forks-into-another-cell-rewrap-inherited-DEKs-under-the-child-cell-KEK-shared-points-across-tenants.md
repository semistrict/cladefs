---
id: TASK-411.4
title: >-
  Forks into another cell rewrap inherited DEKs under the child cell KEK; shared
  points across tenants
status: To Do
assignee: []
created_date: '2026-11-14 18:34'
updated_date: '2026-11-14 18:35'
labels: []
dependencies:
  - TASK-411.3
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 801000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A child reads its parents' objects under their DEKs. Rewrapping them under the child's own cell KEK at the fork means a child never needs another cell's KEK or another tenant's KMS, and shredding a parent leaves children what they inherited, as doc-7 already says of deletion (the user's decision; doc-16, Forks and shared points). It puts a KMS call on the fork's path.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A fork of a point the forker cannot unwrap is refused before any record is made
- [ ] #2 Fork latency with the KMS call is measured and in the task
- [ ] #3 A fork into another cell, and a fork of a shared point into another tenant's cell, reads every inherited extent with only the child's tenant KEK
- [ ] #4 Destroying the parent's tenant KEK after the fork leaves the child fully readable
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
