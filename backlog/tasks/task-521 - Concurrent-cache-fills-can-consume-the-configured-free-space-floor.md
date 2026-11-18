---
id: TASK-521
title: Concurrent cache fills can consume the configured free-space floor
status: Done
assignee: []
created_date: '2026-11-18 12:24'
updated_date: '2026-11-18 17:19'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 911000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 6.

Two different fills can sample the same free space before either write at `crates/bucket_cell/src/cache.rs:435`. Admission at line 447 reserves capacity, but each free-space check covers only its own pending write. With room above the floor for one part, both can be admitted. Account for outstanding disk allocations when admitting both tails and parts.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Concurrent tail and part admissions with space for one allocation preserve the configured free-space floor, including when a write fails.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Fixed by TASK-539.6: the part cache has no free-space floor any more but a limit (its budget and what the disk governor leaves it), against which fills in flight are counted at admission (Index.bytes) under one lock; free_space_admission_reserves_space_for_unwritten_fills, restated against a limit with room for one fill, passes on macOS and Linux.
<!-- SECTION:FINAL_SUMMARY:END -->
