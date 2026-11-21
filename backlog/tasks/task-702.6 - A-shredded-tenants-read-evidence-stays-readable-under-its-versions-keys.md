---
id: TASK-702.6
title: A shredded tenant's read evidence stays readable under its version's keys
status: To Do
assignee: []
created_date: '2026-11-21 01:22'
labels:
  - sandbox
  - encryption
dependencies: []
parent_task_id: TASK-702
priority: medium
ordinal: 1119000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-702.2 encrypts each fork's recording of what its guest read of a version under the version's cell's DEK (bucket_cell::evidence), as every fork of the version must read its tenant's recordings and forks share no DEK of their tenant's. So shredding the forking tenant (ShredTenant) leaves its recordings of other tenants' versions, a public base's among them, readable to whoever holds the version's keys, until the version is dropped or the tenant's newer recordings push them past the newest 8, which a shredded tenant never writes: they stay as long as the version. They say which MiBs of the version its guests read in their first 10 s, not their data.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A tenant shredded, its read evidence of every version, of any cell, is deleted or unreadable (test)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
