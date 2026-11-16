---
id: TASK-471
title: >-
  bench_fork_extent_memory's check that opening and closing the forks again
  leaves the heap to the byte fails by 256-2048 B on main
status: To Do
assignee: []
created_date: '2026-11-16 00:24'
updated_date: '2026-11-16 01:33'
labels:
  - bug
  - flaky
  - perf
dependencies: []
references:
  - TASK-443.2
priority: low
ordinal: 877000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-453 measuring its baseline on main (21f042594, macOS, sim profile, the bench binary run alone, --test-threads 1): remote::tests::bench_fork_extent_memory's assert_eq!(settled(), closed) after reopening and closing every fork failed in 3 of 6 runs: CLADEFS_BENCH_EXTENTS=300000 FORKS=10 (live: left 779426495, right 779426239, 256 B), 1024 x 100 (live: 6899303 vs 6897255, 2048 B), 32768 x 100 (point: 146481963 vs 146479915, 2048 B). 1024 x 10, 32768 x 10 and 300000 x 100 passed. TASK-443.2 settled three holders of 64 B each (page cache's Weak list, tokio's task shards on macOS, extent_store::shared's thread lists); something else still allocates 256-2048 B in the second round and keeps it. It fails the benchmark before its point half runs when the live half trips it, so its numbers are lost for that size. Not a leak shown, but unexplained: the check counts the whole process.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The source of the bytes is named (allocation backtraces over one round, as TASK-443.2 traced them)
- [ ] #2 The check passes 10 runs of 10 at 1024, 32768 and 300000 extents, 10 and 100 forks, or the holder is bounded and the check settles it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 (TASK-453): with the forks sharing what they inherit, the check failed in every run of a point's half alone in a fresh process (CLADEFS_BENCH_PARENTS=point, 6 of 6, +213 to +2,048 B) and once in 6 live runs; with both halves in one process (as main runs them) it passed 3 of 3 at 1,024 x 10, so first-time allocations of a cold process again. The benchmark now runs the check last, after its other assertions. Logs: docs/measurements/macos-fork-extent-memory-shared-2026-11-15/after-*.log.
<!-- SECTION:NOTES:END -->
