---
id: TASK-641
title: >-
  Main's scripts part fails: fxfs-posix's
  a-volume-frees-what-its-snapshot-shares mutant and two guest cocci mutants no
  longer apply
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 18:09'
updated_date: '2026-11-18 20:21'
labels:
  - testing
dependencies: []
priority: high
type: bug
ordinal: 951000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
scripts/test.sh's scripts part (CI's clippy scripts job) fails at origin/main 03b655f63, found merging TASK-539: crates/fxfs-posix/mutants.sh --check reports a-volume-frees-what-its-snapshot-shares.patch broken (40 checked, 1 broken); scripts/tests/test_cocci_guest.py's test_each_rule_finds_the_mutants_it_names_and_each_mutant_has_a_rule_or_a_why fails: guarded-fields.cocci's task-44.19-map-to-unlocked and lock-not-held.cocci's task-44.9-regions-find-unlocked mutants don't apply to guest/linux/fs/cladefs/dax.c (8111fb784 changed it). Each is to be retargeted and still found.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 crates/fxfs-posix/mutants.sh --check reports none broken
- [ ] #2 python3 -m unittest discover -s scripts/tests passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Retarget a-volume-frees-what-its-snapshot-shares at what is_held_beyond_the_store became.
2. Retarget the two guest cocci mutants at today's dax.c; each rule still flags its mutant.
3. fxfs-posix mutants.sh --check none broken; unittest discover -s scripts/tests passes; fxfs-posix mutant killed by its killer.
4. Merge origin/main, push.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18, branch t3/retarget-broken-mutants: a-volume-frees-what-its-snapshot-shares retargeted (base_branches -> base_cells drift) and given a second hunk: since 8648c1dd0 (TASK-241, 23 min after the mutant) apply_mutation's Deallocate arm drops frozen ranges too, so the one-hunk mutant is equivalent: on ubu1 the one-hunk variant was 'not caught' by all of fxfs-posix's tests (257 s); the two-hunk one is killed by model_tests::a_device_that_fails_loses_nothing_durable (58 s). Guest cocci mutants task-44.19-map-to-unlocked and task-44.9-regions-find-unlocked: context drift (branch -> cell, f3cf366ce), regenerated; unittest discover -s scripts/tests 527 OK; guest/sim kills on ubu1: both killed (region_maps_at_once, read_mappings_stay). guest/sim/mutants.sh --check (CI's sim job) found two more broken on main: task-149.15-writeback-batch-short and task-164.2-writeback-holds-no-range (8111fb784's XA_MARK_0 dirty ranges); retargeted (510b3bd9c), the short batch now passing by the marked ranges past its first count. Filed TASK-642: fxfs-posix mutants.sh exits 0 when a test run couldn't start.
<!-- SECTION:NOTES:END -->
