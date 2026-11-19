---
id: TASK-658.1
title: 'Explainer film: encryption at rest and shredding'
status: Done
assignee: []
created_date: '2026-11-19 15:28'
updated_date: '2026-11-19 16:13'
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
- [x] #1 A 45-75 s film shows a tenant key in a KMS wrapping cell keys, ciphertext everywhere data rests, and shredding by destroying the tenant key
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
explainer/encryption/, 62 s: tenant 42's main.rs in cell 9 rests as ciphertext on the host's disk, peers' disks and the bucket under DEK, cell KEK and tenant KEK; shred-tenant or destroying the KEK leaves its cells unreadable, tenant 7 untouched. KMS backends marked in progress (TASK-411.16-.18). Verified: check 0 errors, independent fact check against code (8 fixes), render 62 s at -16.1 LUFS.
<!-- SECTION:FINAL_SUMMARY:END -->
