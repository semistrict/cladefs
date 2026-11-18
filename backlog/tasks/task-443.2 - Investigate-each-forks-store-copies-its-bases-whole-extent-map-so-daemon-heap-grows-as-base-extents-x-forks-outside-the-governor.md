---
id: TASK-443.2
title: >-
  Investigate: each fork's store copies its base's whole extent map, so daemon
  heap grows as base extents x forks, outside the governor
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 12:24'
updated_date: '2026-11-16 22:52'
labels:
  - perf
  - review-2026-11-15
dependencies: []
documentation:
  - docs/reviews/2026-11-15-sandbox-performance.md
parent_task_id: TASK-443
priority: high
ordinal: 836000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/extent_store/src/store.rs Recovering::open collects every extent of the branch, inherited ones too, into a HashMap<ExtentId, Extent> (layout.rs), each with an owned key String (extent_frame::segment::Location). Estimated 150-200 B an extent a fork: ~2 GB at 32k extents and 400 forks, ~20 GB at 300k. It is anonymous memory outside the governor and the anon budget, so the failure is the daemon OOM-killed with every VM. TASK-92.55 slice 4 and TASK-242 name the copy for open time, not memory; TASK-34's 91 KiB a fork was at 64 extents. Found by reading the code, not measured (docs/reviews/2026-11-15-sandbox-performance.md).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cladefsd::tests::serves_hundreds_of_forks_of_a_base (or its like) run with a base of 32,768 and of 300,000 extents: daemon anonymous memory per open idle fork, and the extent count of a real sandbox base image, are recorded
- [x] #2 The notes give the measured cost and the workload size at which a customer would feel it; a fix, if needed, is filed as its own task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Bench on macOS at the layer that runs here: cladefs's host stores over a memory bucket (crates/cladefs/src/remote.rs tests, which already count the heap exactly with crate::heap::Bounded). bench_fork_extent_memory, ignored: for E in {64, 1,024, 32,768, 300,000}, a base of E 4 KiB extents, 100 forks of it, (a) forked from the live store (a checkpoint per fork, as cladefsd's serves_hundreds_of_forks_of_a_base makes them) and (b) forked from a point of it (one state for all); open them idle, read live heap bytes at 1, 10 and 100 open, time each opening; close them, open and close again and assert the heap returns exactly to what the first close left (report what the first close leaves over the baseline).
2. Breakdown by owner, each weighed alone under the same counter: the store's map (a copy of it, through a testing-only accessor of extent_store's), its key strings (sum of lengths), the writer's map (an unshared rebuild of the recovered state's persistent map), the rest by difference.
3. From the code: what the governor and the anon budget count of it.
4. Run in release, results under docs/measurements/macos-fork-extent-memory-2026-11-15/note.md; notes with table, extrapolation (labelled), verdict, how to read a real base's extent count; one fix task if real. Linux daemon-level run (AC #1) is the parent's: left unchecked.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15, measured on macOS (Apple M5 Pro; sim profile; fxdax remote::tests::bench_fork_extent_memory: host stores over a memory bucket in one process, heap counted exactly by the test binary's counting allocator; docs/measurements/macos-fork-extent-memory-2026-11-15/note.md and bench.log). Run: cargo nextest run -p fxdax --cargo-profile sim --run-ignored only bench_fork_extent_memory --no-capture.

Bytes of heap per additional open idle fork (forks 51-100; exact), and per base extent:
| base extents | fork of a point | B/extent | fork of a live store | B/extent |
| 64 | 46,085 | 720 | 60,725 | 949 |
| 1,024 | 273,633 | 267 | 508,586 | 497 |
| 32,768 | 7,796,987 | 238 | 15,333,008 | 468 |
| 300,000 | 66,209,810 | 221 | 135,208,456 | 451 |
Measured totals at 100 forks: 32,768: 0.78 GB / 1.41 GB; 300,000: 6.62 GB / 12.42 GB. All of E in {64, 1,024, 32,768, 300,000} x F in {1, 10, 100} reached. Point forks cost the same at F = 1, 10, 100; a live store's first 16 forks read as the store's map alone because each displaces a decoded checkpoint the bucket's Decoded cache (KEEP 16) already held (forks 11-20 average exactly 6 cheap + 4 full).

Breakdown per extent at 32,768: the store's HashMap (extent_store Recovering::open) 130 B table + 107 B key String = 237 B, 99.6% of a point's fork; a fork of a live store also holds its writer's imbl OrdMap unshared, 123 B nodes + 107 B key = 230 B (forks of a point share the point's one). Fixed ~31 KB a fork. Decoded keeps 16 states of any size (1.1 GB at 300,000 extents).

Close: heap returns to the byte on a second open-and-close of all forks (asserted); the first close leaves 70-81 KB once, not per fork. No leak.

Open time per fork, machine quiet: 3.4-5 ms at 64 and 1,024; 5.7-8.7 ms (point) and 8.5-12 ms (live) at 32,768; 39-53 ms and 69-93 ms at 300,000. bench.log's times are from a rerun under other agents' load and are not a measurement.

Counted by nothing (from the code): the governor limits extent-cache pages and held parts only; AnonBudget has no part for it (it would be OWN, 64 MiB), and its framings part adds 288 MiB per open store, so 400 idle forks raise the budget 112.5 GiB and no sample goes over.

EXTRAPOLATION (bytes a fork x forks; linear as measured to 100): 400 forks: 32,768: 7,796,987 x 400 = 3.1 GB (point), 15,333,008 x 400 = 6.1 GB (live); 300,000: 66,209,810 x 400 = 26.5 GB, 135,208,456 x 400 = 54.1 GB; 1,024: 109 MB / 203 MB; 64: 18 MB / 24 MB. 1,000 forks: 32,768: 7.8 GB / 15.3 GB; 300,000: 66.2 GB / 135.2 GB; 1,024: 274 MB / 509 MB; 64: 46 MB / 61 MB.

Verdict: real, and worse than the review's 150-200 B: 221-238 B an extent a fork, 451-468 B for forks of a live store, uncounted. A host feels it when base extents x open forks reaches ~10^7 (2.4-4.7 GB): e.g. 400 forks of a 32,768-extent base. Harmless at 1,024 extents. Fix filed: TASK-453.

The extent count of a real sandbox base is NOT known and not guessed: nothing reports it (fxdaxctl stat prints bytes only; extent_store::Store::extents() has it, unexposed). Filed TASK-454 to print it; until then the parent can read it on ubu1 only with code (e.g. a test or tool calling Store::extents().len() on the base's store).

AC #1 is NOT met and left unchecked: the Linux daemon-level run (serves_hundreds_of_forks_of_a_base at 32,768 and 300,000; RssAnon, glibc overhead) and the real base's count were not done here (no ubu1 in this task). AC #2 checked: cost, the size it bites at, fix filed. DoD unchecked: only extent_store + fxdax ran on macOS (641 passed), not the workspace, not Linux.

2026-11-15, rebased onto main as of TASK-433: bench_fork_extent_memory's check that opening and closing the forks again leaves the heap to the byte failed by 64 bytes, now and then. Traced with a counting allocator recording each live allocation's backtrace over one round (instrumentation removed again). None is a leak, and none is the stores'; three holders, each bounded: (1) the page cache's list of files (extent_store::pages, TASK-433) names closed files by Weak, whose ArcInner (176 B each) stays until the list is next pruned, by the flusher every quarter of Options::expire or by write_back_all; (2) on macOS, each of the 32 shards of the remotes' runtime's tokio list of tasks (8 workers x 4) boxes its pthread mutex, 64 B, the first time it is locked, the shard being the task id, numbered across the process, & 31; a round's uploaders reached shards none had; (3) extent_store::shared's thread-local list of the shared locks a thread holds allocates 64 B at a thread's first lock, and the runtime's blocking threads (block_in_place hands a worker to one) took their first lock in a later round, timing-dependent. Fixed in the benchmark's commit: settled() prunes (1); every_task_shard_locked() takes each shard once before the count (2); the remotes' runtime's threads make their list as they start, shared::prepare_thread through on_thread_start (3). Each taken out alone: (2) red 3 of 3 runs, (3) red 1 of 3. Passes at 64, 1,024 and 32,768 extents, forks 1 and 10 (six runs at 64,1024). The structure: no class to remove in the stores' code; the check counts the whole process, so what a process's shared runtime and threads first make shows in it, and the benchmark has them made before it counts.
<!-- SECTION:NOTES:END -->
