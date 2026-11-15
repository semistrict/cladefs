---
id: TASK-431
title: >-
  extent_store crash test recovers_from_its_bucket_with_and_without_its_disk:
  commits spanning segments at its bound (20 of 100 rounds, needs > 20)
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 09:26'
updated_date: '2026-11-15 10:50'
labels:
  - testing
  - flaky
dependencies: []
priority: high
ordinal: 822000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main's CI test (workspace) on 7edc54ef3 (run 43682804204) failed crates/extent_store/tests/crash/main.rs: '20 commits spanned segments in 100 rounds' (assert spanning > KEPT_ROUNDS / 5); passed on 1fd0855fb. A coverage assertion sitting at its bound: find whether TASK-400 (parts packed ahead), TASK-407 or TASK-411 changed how often commits span segments, and make the test produce spanning commits deterministically enough (seeded workload sized for it) rather than lowering the bound.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Measure the kept test's spanning count (a local eprintln, not committed) over runs at HEAD and at 1e3253239 (before TASK-400), c2daf7630 (after TASK-400, before TASK-411.8), 845da8f57 (after TASK-411, before TASK-407), 8 runs each, on the Mac, nothing else running.
2. Cause: a commit spans segments only when a segment fills between two commits: more than segment_limit (16 + 48 KiB) written since it opened. The random workload does that rarely; the uploader's interval closing (1-20 ms, wall clock) resets the open segment once per commit, so the count follows the host's speed.
3. Fix: rounds.rs's workload gets a burst op, BURST extents of BURST_BLOCKS blocks (under large_extent, so in the segment), more than two segment_limits, then a commit: a segment fills whatever the open one held and though an uploader closes it once among them. Refactored the workload's arms into a Run struct's methods.
4. Show the count's distribution over 20+ runs after; run all three crash tests and the Shuttle crash tests (rounds shared).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Cause: a commit spans segments only when a segment fills between two commits, more than segment_limit (16 + 48 KiB) written since it opened; the random workload does so only in long runs of writes, which an uploader's interval closing (it closes the open segment once a commit, when it fires) or the workload's own close_now cut short. The kept rounds made ~30 a run of 100 (bound > 20), with the run-to-run spread of their uploader's timing.
Measured on the Mac (spanning per run of the kept test, 8 runs each, nothing else running): 1e3253239 before TASK-400: 30 30 32 29 29 32 31 33 (mean 30.8); c2daf7630 after TASK-400: 31 31 31 31 31 28 32 32 (30.9); 845da8f57 after TASK-411, before TASK-407: 27 30 29 28 32 29 33 30 (29.8); HEAD 2fe3a766b: 31 32 32 31 31 32 28 33 30 29 30 29 29 26 31 (30.3, 15 runs). No commit moved it; every core busy with spinners didn't either (30 31 32 31). CI's 20 (run 43682804204, under the workspace's parallel run on its Linux runner) is the low tail there; not reproduced on the Mac.
Fix: rounds.rs's workload has a burst (a quarter of op 14's draws): BURST = 7 extents of BURST_BLOCKS = 5 blocks (20 KiB, under large_extent, so in the open segment), more than two segment_limits, then a commit: a segment fills whatever the open one held, though an uploader closes it once among them, and the commit spans segments. The workload's arms became Run's methods (write_new, write_more, free, commit, flush, burst). Bound unchanged.
After (Mac): kept test 26 runs alone, all pass: 103 104 104 104 104 104 104 105 105 105 105 106 106 106 106 106 106 106 107 107 107 107 107 107 108 110; with every core busy: 104 102 104 103. Serial rounds: spanning 774 -> 3184 (bound > 300), crashes mid-commit 250 -> 236 (bound > 30), 40 s -> 57 s. Shuttle crash test passes, 52 s -> 71 s. openings_cut passes (11 s). Kept test 30 s -> 34 s. A burst at every draw of op 14 gave 226 a run but the serial test took 111 s: a quarter keeps the margin (5x the bound) at less cost. Also found and fixed TASK-432 (the crash copy race, 'lost'), which failed this test 1 run in ~15. clippy (extent_store --tests) clean in tests/crash; rustfmt clean. scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The kept rounds' spanning commits sat at their bound (~30 a run on the Mac at every commit measured, before TASK-400 through HEAD; 20 once in CI) because only long write runs fill a segment between commits, which uploader closings cut short. The shared crash workload now has bursts, 7 x 20 KiB extents then a commit, more than two segment limits, so a segment always fills: 103-110 in 26 runs alone, 102-104 under full load; bound unchanged; all crash and Shuttle crash tests pass.
<!-- SECTION:FINAL_SUMMARY:END -->
