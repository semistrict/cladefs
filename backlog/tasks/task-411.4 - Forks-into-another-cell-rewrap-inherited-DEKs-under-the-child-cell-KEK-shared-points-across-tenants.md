---
id: TASK-411.4
title: >-
  Forks into another cell rewrap inherited DEKs under the child cell KEK; shared
  points across tenants
status: To Do
assignee: []
created_date: '2026-11-14 18:34'
updated_date: '2026-11-14 23:22'
labels: []
dependencies:
  - TASK-411.3
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: high
type: task
ordinal: 801000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A child reads its parents' objects under their DEKs. Rewrapping them under the child's own cell KEK at the fork means a child never needs another cell's KEK or another tenant's KMS, and shredding a parent leaves children what they inherited, as doc-7 already says of deletion (the user's decision; doc-16, Forks and shared points). A new cell's KEK comes from a per-host pool of KEKs the KMS wrapped ahead of time (the user's decision), so the fork's path makes no KMS call.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A fork of a point the forker cannot unwrap is refused before any record is made
- [ ] #2 A fork into another cell, and a fork of a shared point into another tenant's cell, reads every inherited extent with only the child's tenant KEK
- [ ] #3 Destroying the parent's tenant KEK after the fork leaves the child fully readable
- [ ] #4 A fork into a new cell makes no KMS call: its cell KEK comes from the host's pool, refilled in batches off the fork's path; a test counts calls
- [ ] #5 Revoking or destroying a tenant's KEK discards its pool: the next fork into a new cell of that tenant is refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Since TASK-411.3, fxdax makes every fork, point and restore of a sealed store in a new cell, so all of them fail ENOTSUP until this task rewraps inherited DEKs.
<!-- SECTION:NOTES:END -->
