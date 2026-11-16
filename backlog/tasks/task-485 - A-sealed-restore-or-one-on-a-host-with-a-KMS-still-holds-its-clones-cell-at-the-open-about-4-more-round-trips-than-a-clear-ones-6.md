---
id: TASK-485
title: >-
  A sealed restore, or one on a host with a KMS, still holds its clone's cell at
  the open: about 4 more round trips than a clear one's 6
status: To Do
assignee: []
created_date: '2026-11-16 06:41'
updated_date: '2026-11-16 14:34'
labels:
  - perf
dependencies: []
references:
  - TASK-449
priority: medium
ordinal: 881000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-449 cut a restore (a fork of a point and its child's first open) to 7 requests in 6 round trips one after another, on a host with no KMS forking a point in the clear: the fork holds the child's cell, made, at once with the reference, and the child's store's first opening starts from what the fork left (extent_remote::known). Where the host has a KMS, or the point's origin is sealed (Remote::inheriting gives keys), the fork holds no cell: keys::give_inherited makes the cell unheld (token 0) and gives the record its keys after the fork is made, and the opening reads the cell, swaps it to its placement, then takes the head and reads the record. Read from the code, not measured: the fork's 4 round trips plus give_inherited's 2, the open's 4, about 10 in series against 6. Sealed tenants are the ones agent-sandbox customers most likely run.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 bucket_lifecycle_by_phase or a test of its kind measures a sealed restore (a KMS host, a sealed point) by phase: requests and round trips in series
- [ ] #2 A sealed restore costs no more round trips in series than a clear one's plus what giving the keys needs, shown by that measurement and pinned by a test
- [ ] #3 specs/bucket or doc-7 says why any reordering of the cell's making, the keys' giving and the head's taking is safe
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Measured (TASK-411.28's merge with TASK-449, restores_ask_the_bucket_what_the_protocol_needs, which now pins the encrypted restore every restore is): point-open 7 requests in 6 round trips (the point's cell, the base's record and cell read for the keys), fork 7 in 6 (give_inherited after the record: the cell created with a pooled KEK, the record read and updated with its DEKs), fork-open 7 in 7 (the cell read twice, the base's record twice: TASK-460). A restore 14 requests in 13 round trips against a clear one's 8 in 6. TASK-449's clear-cell path for KMS-less hosts is gone: every host has a KMS.
<!-- SECTION:NOTES:END -->
