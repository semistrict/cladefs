---
id: TASK-420
title: >-
  Flaky: nextest flags Shuttle tests leaky now and then on macOS (follower
  busy_by_pct, extent_store needs_closes_to_wait_for_framings)
status: To Do
assignee: []
created_date: '2026-11-15 02:08'
updated_date: '2026-11-15 20:25'
labels:
  - testing
  - flaky
dependencies: []
priority: low
type: bug
ordinal: 808000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-400's runs of scripts/test.sh's shuttle part on macOS (2026-11-15): two runs each flagged one Shuttle test LEAK though it passed: follower::shuttle busy_by_pct (0.69 s), then extent_store::shuttle needs_closes_to_wait_for_framings (0.23 s), a different test each run; the second passed 5/5 without the flag rerun alone. Under the shuttle feature the store starts no thread of its own outside Shuttle's schedule (TASK-400's packing ahead packs inline there), so it's likely the harness or nextest's leak heuristic (a child holding the test's stdout/stderr past its exit). Find what holds the handles, or say why it's benign.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause of the LEAK flags is found and fixed, or shown harmless and recorded
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 (claude, TASK-423): a macOS shuttle part run flagged follower_sim sim::tests::catches_followers_skipping_changes LEAK (0.96 s), passed: a third test, same flag.

2026-11-15 (claude, TASK-423): cargo nextest run -p extent_store -p fxdax on macOS flagged 5 non-Shuttle extent_store tests LEAK at ~0.21 s each, early in the run (durability batches_take_the_commits_queued_in_order, syncs_sync_each_file_once_then_the_directories, framed files_close_the_least_lately_read_first, seals pieces_say_why, corrupt flipped_bytes_fail_reads_and_seals_of_open_extents_in_closed_segments), all passed: the leak flag is not Shuttle's alone.

2026-11-15, another, not a Shuttle test: nextest flagged cladefs filesystem::tests::a_write_given_up_on_completes_before_the_unmount LEAK (passed, 0.228 s) in cargo nextest run -p cladefs -p extent_store -p extent_remote on macOS, branch t3/proactive-performance-reliability rebased onto main 76fabab76 (915 passed, 1 leaky); the branch touches no filesystem code. Not looked into: a handle of the test's process left open past its end, as with the others.
<!-- SECTION:NOTES:END -->
