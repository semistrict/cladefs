---
id: TASK-658.1
title: 'Explainer film: encryption at rest and shredding'
status: To Do
assignee: []
created_date: '2026-11-19 15:28'
labels:
  - explainer
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-vdisk-DEKs.md
  - docs/operating-a-host.md
parent_task_id: TASK-658
type: docs
ordinal: 1039000
---

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A 45-75 s film shows a tenant key in a KMS wrapping cell keys, ciphertext everywhere data rests, and shredding by destroying the tenant key
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
