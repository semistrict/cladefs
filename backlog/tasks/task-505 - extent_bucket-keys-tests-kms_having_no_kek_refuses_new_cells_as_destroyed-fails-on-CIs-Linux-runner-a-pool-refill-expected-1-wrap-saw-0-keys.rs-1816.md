---
id: TASK-505
title: >-
  extent_bucket keys::tests::kms_having_no_kek_refuses_new_cells_as_destroyed
  fails on CI's Linux runner: a pool refill expected 1 wrap, saw 0
  (keys.rs:1816)
status: To Do
assignee: []
created_date: '2026-11-17 02:10'
updated_date: '2026-11-18 01:37'
labels:
  - bug
  - test
dependencies: []
ordinal: 892000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Fails on CI's test (workspace) job on main (run of 001032beb, job 133692118967) and on TASK-503's branch (run 43993691140): assertion left 0, right 1 at crates/extent_bucket/src/keys.rs:1816, in the loop counting the KMS's wraps for tenant 2, which has no KEK. It passes on macOS and on ubu1 (TASK-503's full suites). The pooled CachingKms refills in the background on a multi-thread runtime, so the count likely races the refill; not investigated.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The test passes on CI's Linux runner in 20 runs, or the race it hits is fixed and named
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18: seen on ubu1 too, once in a whole-workspace nextest under load (branch t3/cell-is-one-filesystem); 6 reruns alone passed.
<!-- SECTION:NOTES:END -->
