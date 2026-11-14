---
id: TASK-411.6
title: >-
  The collector compacts encrypted objects with keys through the KMS; marking
  stays keyless
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
type: task
ordinal: 803000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
collector::compact_object copies extent ranges out of decompressed data, so it must decrypt and re-encrypt under the source branch's DEK with a new salt. The user chose giving the collector KMS access over part-granular keyless compaction (doc-16). Marking reads only metadata, which stays in the clear.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A pass compacts an encrypted cell and readers read the moved extents
- [ ] #2 A pass that only marks, or finds nothing to compact, makes no KMS call: a test counts calls
- [ ] #3 A cell whose tenant KEK is revoked is marked and has unreferenced objects deleted, but is not compacted, and the pass reports it
- [ ] #4 A sealed moves object copied to another key of its branch is refused (TASK-411.3 wrote the check, untested until moves are sealed)
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
