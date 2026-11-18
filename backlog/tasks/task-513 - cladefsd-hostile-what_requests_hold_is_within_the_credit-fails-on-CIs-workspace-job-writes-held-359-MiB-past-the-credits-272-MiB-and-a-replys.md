---
id: TASK-513
title: >-
  cladefsd hostile what_requests_hold_is_within_the_credit fails on CI's
  workspace job: writes held 359 MiB past the credit's 272 MiB and a reply's
status: To Do
assignee: []
created_date: '2026-11-18 04:32'
labels:
  - bug
  - flaky
dependencies: []
priority: medium
ordinal: 898000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CI run on main 558c354e2 (test (workspace)): assertion at crates/cladefs/src/cladefsd/tests/hostile.rs:2204: MiB held past the credit's 272 MiB and a reply's: roots 0, reads 322, reads into the area 8, writes from the area 280, writes 359 (false). On ubu1, alone under nextest, it passed 3 of 3 on 073feb370 (before), 0559585cb (with TASK-444.13's heap counting) and a variant of the heap counting: load-dependent. The heap's peak (heap::peak_while) counts the whole process's allocations, so what a busy runner's other threads of the test hold, or allocator timing, lands in it; reads at 322 MiB is past the bound too yet counted within (true), so the bound's margin is thin under load.
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
