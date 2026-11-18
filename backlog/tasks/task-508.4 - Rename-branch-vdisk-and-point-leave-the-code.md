---
id: TASK-508.4
title: 'Rename: branch, vdisk and point leave the code'
status: To Do
assignee: []
created_date: '2026-11-18 00:11'
labels:
  - bucket
  - host
  - guest
dependencies: []
documentation:
  - doc-20
parent_task_id: TASK-508
ordinal: 901000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-20 retires branch, vdisk, point, slot, namespace and cut. Once the structure is one filesystem per cell, the remaining names (types, fields, protos, the extent ID's branch, Fxfs's uses of it, the guest kernel's vext_id, docs) follow.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 rg -i for vdisk, branch, point, slot, namespace and cut finds only the exceptions doc-20 lists
- [ ] #2 Every test suite passes, on macOS and on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
