---
id: TASK-399
title: >-
  extent_store's corrupt test closed_segments_with_flipped_bytes_are_not_taken
  fails under load: its segment file not there yet
status: Done
assignee:
  - '@claude'
created_date: '2026-11-13 14:04'
updated_date: '2026-11-15 22:12'
labels:
  - bug
  - tests
dependencies: []
priority: low
ordinal: 784000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen once on macOS (2026-11-13, TASK-395 batch, the workspace's tests running beside three simulation searches): crates/extent_store/tests/corrupt.rs:90, stored() opening segments/0000000000000007/0000000000000000 as Place::Run named it: 'doesn't exist' (MemDir, NotFound). Five runs alone after passed. Likely the test reads the segment's file before the store's framing thread (or rename into place) has written it: a race in the test's wait, or a place the store reports before its file is there. Not touched by TASK-395 (no extent_store store/segment code changed). Reproduce under load (e.g. stress-ng or several sim searches beside it) and find which.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The test waits on what makes the file exist, or the store names no place whose file isn't there
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15, coordinator: failed main's CI run 43762655434 (464e6e201) the same way. Cause: flush returns before make_durable renames framing-N to N; stored() opens N. Fix: wait_idle() (TASK-463) before stored(). Proof: segments::making_durable fail point at sleep(300): old test FAILS, fixed PASSES; 60 runs under load 0 failures; extent_store 291/291 on the Mac.

2026-11-15, coordinator: main's CI run 43765433746 on cd8d9a286 green on every job (test (workspace), macos (workspace), guest, clippy scripts, test-weakening, sandboxes among them).
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
closed_segments_with_flipped_bytes_are_not_taken waits for the store to be idle (wait_idle, TASK-463) before opening the framed file, which the thread making framings durable renames into place after the flush returns (70c9e50ad). Verified: with segments::making_durable held 300 ms the old test fails and the new passes; 60 runs under load 0 failures; extent_store 291/291; main's CI green (run 43765433746).
<!-- SECTION:FINAL_SUMMARY:END -->
