---
id: TASK-513
title: >-
  cladefsd hostile what_requests_hold_is_within_the_credit fails on CI's
  workspace job: writes held 359 MiB past the credit's 272 MiB and a reply's
status: To Do
assignee: []
created_date: '2026-11-18 04:32'
updated_date: '2026-11-18 06:35'
labels:
  - bug
  - flaky
dependencies: []
priority: medium
ordinal: 898000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CI run on main 558c354e2 (test (workspace)): assertion at crates/cladefs/src/cladefsd/tests/hostile.rs:2204: MiB held past the credit's 272 MiB and a reply's: roots 0, reads 322, reads into the area 8, writes from the area 280 (all within the bound, credit(0) + MAX_PAYLOAD), writes 359 (past it). On ubu1, alone under nextest, it passed 3 of 3 on 073feb370 (before), 0559585cb (with TASK-444.13's heap counting) and a variant of the heap counting: load-dependent. The heap's peak (heap::peak_while) counts the whole process's allocations, so what else of the test's process holds meanwhile, or allocator timing, lands in it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The test bounds what the daemon holds of a burst without depending on the runner's load, or fails only when the daemon holds more
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
CI history with TASK-444.13's heap counting: failed on 558c354e2 and 08dc77a00, passed on 457603481 and ddc9ac1f7; passed before on 073feb370 and ad2b6a371. Intermittent, 2 of 4 since; whether the counting (the old allocation held until realloc returns, not released before) raises the peak under load is open.
<!-- SECTION:NOTES:END -->
