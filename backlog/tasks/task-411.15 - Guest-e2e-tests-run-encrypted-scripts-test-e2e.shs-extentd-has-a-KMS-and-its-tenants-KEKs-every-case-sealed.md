---
id: TASK-411.15
title: >-
  Guest e2e tests run encrypted: scripts/test-e2e.sh's extentd has a KMS and its
  tenants KEKs, every case sealed
status: To Do
assignee: []
created_date: '2026-11-15 03:35'
updated_date: '2026-11-15 11:35'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
priority: high
type: task
ordinal: 813000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-411's AC #3: the VMM and guest end-to-end paths (scripts/test-e2e.sh, guest/tests) have no encrypted variant. The user chose (2026-11-14) that e2e runs encrypted only: no clear e2e runs; plaintext stays covered by the host-side suites. Today test-e2e.sh starts extentd with no CLADEFS_KMS, so every guest's store is a plaintext cell.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 scripts/test-e2e.sh starts extentd (and every process of the cases that opens stores: cladefsctl, cladefs-gc, FUSE mounts) with CLADEFS_KMS naming a LocalKms holding a KEK for every tenant the cases use, tenant 0 included or the cases moved to granted tenants
- [ ] #2 A check in the harness fails a run whose stores are not sealed: every cell the run made has a wrapped KEK and every branch record a DEK, and no object of the bucket nor file of the stores' directories holds a known plaintext a case wrote
- [ ] #3 The e2e suite passes encrypted on ubu1 (test-e2e.sh, --tag e2e), and the nightly/qualification scripts that run it (scripts/qualify.sh, bench scripts) run encrypted too
- [ ] #4 Guest checks' time budgets hold encrypted, or the cases that overrun are named with their measured times
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
