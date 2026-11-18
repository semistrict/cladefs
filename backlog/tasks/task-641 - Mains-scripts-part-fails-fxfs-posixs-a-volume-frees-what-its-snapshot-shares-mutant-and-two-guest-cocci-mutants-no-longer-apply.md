---
id: TASK-641
title: >-
  Main's scripts part fails: fxfs-posix's
  a-volume-frees-what-its-snapshot-shares mutant and two guest cocci mutants no
  longer apply
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 18:09'
updated_date: '2026-11-18 20:56'
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
- [x] #1 crates/fxfs-posix/mutants.sh --check reports none broken
- [x] #2 python3 -m unittest discover -s scripts/tests passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Retarget a-volume-frees-what-its-snapshot-shares at what is_held_beyond_the_store became; check whether one hunk is still killed.
2. Retarget the two guest cocci mutants at today's dax.c; each rule still flags its mutant; and the two guest/sim mutants guest/sim/mutants.sh --check found broken.
3. fxfs-posix and guest/sim mutants.sh --check none broken; unittest discover passes; each mutant killed by its killer on ubu1.
4. Merge origin/main, push.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18, branch t3/retarget-broken-mutants: a-volume-frees-what-its-snapshot-shares retargeted (base_branches -> base_cells drift) and given a second hunk: since 8648c1dd0 (TASK-241, 23 min after the mutant) apply_mutation's Deallocate arm drops frozen ranges too, so the one-hunk mutant is equivalent: on ubu1 the one-hunk variant was 'not caught' by all of fxfs-posix's tests (257 s); the two-hunk one is killed by model_tests::a_device_that_fails_loses_nothing_durable (58 s). Guest cocci mutants task-44.19-map-to-unlocked and task-44.9-regions-find-unlocked: context drift (branch -> cell, f3cf366ce), regenerated; unittest discover -s scripts/tests 527 OK; guest/sim kills on ubu1: both killed (region_maps_at_once, read_mappings_stay). guest/sim/mutants.sh --check (CI's sim job) found two more broken on main: task-149.15-writeback-batch-short and task-164.2-writeback-holds-no-range (8111fb784's XA_MARK_0 dirty ranges); retargeted (510b3bd9c), the short batch now passing by the marked ranges past its first count. Filed TASK-642: fxfs-posix mutants.sh exits 0 when a test run couldn't start.

Validation: fxfs-posix mutants.sh --check 40/0 broken, guest/sim mutants.sh --check 45/0 (ubu1), unittest discover -s scripts/tests 527 OK (after merging origin/main); kills on ubu1: a-volume-frees-what-its-snapshot-shares by model_tests::a_device_that_fails_loses_nothing_durable, task-44.19-map-to-unlocked by region_maps_at_once, task-44.9-regions-find-unlocked by read_mappings_stay, task-149.15-writeback-batch-short by cow_writeback_counts_again (fsync left a chunk out), task-164.2-writeback-holds-no-range by cow_reclaim_during_writeback (ASan heap-use-after-free). Workspace: macOS 2641/2641, Linux 2962/2962 (ubu1, fxjob). upstream-changes.py --check passes. No upstream deviation: only mutant patches changed.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Retargeted fxfs-posix's a-volume-frees-what-its-snapshot-shares (base_branches -> base_cells, plus TASK-241's apply-side frozen guard, without which the one-hunk mutant survives all of fxfs-posix's tests) and four guest mutants at today's dax.c (cocci's task-44.19-map-to-unlocked and task-44.9-regions-find-unlocked; guest/sim's task-149.15-writeback-batch-short and task-164.2-writeback-holds-no-range, at 8111fb784's marked dirty ranges). Each --check reports none broken, the scripts' unittests pass, and every one is killed by its killer on ubu1. Pushed to main (b454b96f6).
<!-- SECTION:FINAL_SUMMARY:END -->
