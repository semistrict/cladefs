---
id: TASK-423
title: >-
  extent_store's extents_written_in_runs_seal_where_they_are flakes since
  TASK-400: the commit's I/O window counts 1 MiB read, the look-ahead packing a
  part beside it
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 05:06'
updated_date: '2026-11-15 05:51'
labels:
  - testing
  - flaky
  - bug
dependencies: []
priority: high
ordinal: 817000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main's CI test (workspace) on 7998d1f86 (run 43675449547): crates/extent_store/tests/store.rs extents_written_in_runs_seal_where_they_are, assert_eq!((sealed.read - before.read, sealed.written - before.written), (0, 0)) failed with left (1048576, 0). The test measures MemDir's I/O across store.commit_ordered to prove the seal reads and writes nothing; since c2daf7630 (TASK-400) a segment's parts are read and packed ahead on idle-priority threads as they are written whole, asynchronously, so a 1 MiB part read by the look-ahead can land inside the window. Either the test must not count the look-ahead's reads (wait for it to settle before measuring, or measure only the commit's own I/O), or the look-ahead must not read what the commit is sealing; decide which the code promises. Not the seal reading back (TASK-92.1).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The test passes deterministically: 0 failures over many runs under load, and a run that forces the look-ahead into the window fails without the fix
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Decide the promise: the look ahead (TASK-400) reads each part written whole, queued by the writes, on the framer, whatever comes next; it is not the commit's I/O, and nothing promises it doesn't overlap a commit (it must not wait for one, nor a commit for it). So fix the test side: measure once no look ahead is queued or reading.
2. Store::wait_looked_ahead (tests' API, beside wait_framed): waits on the store's condvar while Framing::looks_ahead(): a Job::See queued, or Framing::looking, set by Framer::see across its lock-free read, cleared and notified once it saw.
3. extents_written_in_runs_seal_where_they_are waits for it before measuring.
4. Sweep extent_store's and fxdax's I/O-measuring tests (dir.io(), framing_reads, held counts) for the same race.
5. Prove: temporary failpoints holding the look ahead until the commit begins (and the commit until it read) fail the unfixed test deterministically; unfixed vs fixed under 15 spinning processes on the Mac, nextest --stress-count.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 (claude): root cause: the writes' last finish queues Job::See for parts 0 and 1 (each 1 MiB written whole); on a concurrent MemDir the framer thread reads them (read_cached -> MemFile::read_at, counted in Io::read) with no lock held, whenever it gets the CPU, so under load a read lands between the test's two dir.io() calls around commit_ordered: CI's (1048576, 0). Not the seal reading back (TASK-92.1): the seal still reads/writes 0.
Fix (test side, the look ahead promises no ordering against a commit and must not wait for one): Store::wait_looked_ahead + Framing::looking/looks_ahead (state.rs, store.rs); the test waits before measuring.
Proof (Mac, M-series, 15 CPUs): temporary failpoints (look waits for the commit to begin; commit waits for one look's read; applied, run, reversed, not committed): unfixed test fails deterministically with left (1048576, 0), CI's; fixed test passes with the hooks compiled in, unconfigured. Under 15 'yes' spinners: unfixed copy 10/1000 failed (all (1048576, 0)); fixed 0/2000.
Sweep: no other test races. store.rs opened_reading/eight_segments write blocks only (no part written whole, no look ahead); synced_dirs/synced_files/most_unwritten tests count syncs/writes (a look ahead does neither); segments_framed_in_copies_take_their_parts_held and encrypted.rs sealed_stores_hold_no_parts_packed_ahead assert reads on MemDir::serial (looks inline), the concurrent variant asserts only bytes; segments.rs unit tests look ahead inline; fxdax: no dir.io(); extentd's governor test writes noise (nothing held). store-bench's opening read count can include a closed store's in-flight look: filed TASK-426 (close returns with a look ahead still reading).
Verified on the Mac: clippy part (workspace + shuttle builds), shuttle part green (follower_sim LEAK flag, noted on TASK-420), cargo nextest -p extent_store -p fxdax (but sim) 592/592 (5 LEAK flags, noted on TASK-420), upstream-changes.py --check.

2026-11-15 (claude): pushed to main as 7edc54ef3 (rebased on c8344a37a). Rebased tree on the Mac: clippy + shuttle parts green (LEAK flags only: fxdax dax stores_race_frees_*, follower busy_by_pct, follower_sim failure::*, TASK-420's), extent_store + fxdax 591/591, upstream-changes.py --check. No upstream code touched (DoD #3). DoD #1's Linux half: Mac was enough (the race isn't Linux-only); main's CI run 43682804204 on 7edc54ef3 was pending when last looked at.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The test measured the framer's look-ahead reads (TASK-400: parts written whole, read on the framer thread with no lock, whenever it ran) as the commit's. The look ahead promises no order against a commit, so the test changed: Store::wait_looked_ahead waits while a look ahead is queued or reading (Framing::looking), and the test waits before measuring. Forced with temporary failpoints the old test failed every run with CI's (1048576, 0); under 15 spinners old 10/1000, new 0/2000. No other I/O-measuring test races; TASK-426 filed for close returning mid-look.
<!-- SECTION:FINAL_SUMMARY:END -->
