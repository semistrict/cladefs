---
id: TASK-460
title: Takes of sealed branches read their cell's object twice
status: To Do
assignee: []
created_date: '2026-11-15 13:51'
updated_date: '2026-11-15 19:11'
labels:
  - perf
  - encryption
dependencies: []
ordinal: 829000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Every branch is sealed since TASK-411.28, so every take of a branch with DEKs goes the path sealed takes had: upload.rs reads the cell's object for its KEK to recover the branch under (cell_keys with the placement), then cell::hold reads it again before its compare-and-swap. A take now asks 6 GETs where a clear one asked 5 (cladefs remote::tests::takes_recover_their_branches_once_by_their_segments_tails, whose expectation TASK-411.28 raised). One read of the object could serve both, the hold swapping from it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A take of a sealed branch reads its cell's object once
- [ ] #2 takes_recover_their_branches_once_by_their_segments_tails expects 5 GETs and one cell object's bytes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
