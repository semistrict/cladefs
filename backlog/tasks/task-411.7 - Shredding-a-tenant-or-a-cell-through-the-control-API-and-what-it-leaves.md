---
id: TASK-411.7
title: 'Shredding a tenant or a cell through the control API, and what it leaves'
status: To Do
assignee: []
created_date: '2026-11-14 18:34'
labels: []
dependencies:
  - TASK-411.3
  - TASK-411.4
  - TASK-411.5
documentation:
  - >-
    backlog/docs/doc-16 -
    Per-tenant-encryption-at-rest-tenant-KEKs-cell-KEKs-volume-DEKs.md
parent_task_id: TASK-411
type: task
ordinal: 804000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Crypto-shredding is one of the three goals (doc-16). A tenant shreds by its KEK in the KMS; a cell by removing its wrapped KEK, which is only as strong as the bucket keeps no older versions of the cell object.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Shredding a tenant leaves none of its volumes readable on any host, including open segments and cached parts: an end-to-end test
- [ ] #2 Shredding a cell leaves its volumes unreadable and other cells of the tenant readable
- [ ] #3 doc-16 and the operator docs say which bucket lifecycle rules a cell shred needs
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
