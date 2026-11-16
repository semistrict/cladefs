---
id: TASK-470
title: >-
  virtio_extent whatever_a_driver_asks_is_answered fails on Linux: the test
  store's write overflows at offset + buffer length
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 23:44'
updated_date: '2026-11-16 06:25'
labels:
  - bug
  - testing
dependencies: []
priority: medium
ordinal: 876000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-15 by the Linux workspace run on ubu1 (branch perf-linux at 618403778, which does not touch crates/virtio_extent): the proptest requests::tests::whatever_a_driver_asks_is_answered panicked "attempt to add with overflow" in the test store's write (crates/virtio_extent/src/requests.rs, `data.resize(data.len().max(at + buffer.len()), 0)` with `let mut at = offset as usize;`), then every later case failed on the poisoned mutex (requests.rs `self.state()`); shrinking gave up after 8000 iterations; minimal input not found. A driver's WRITE with an offset near u64::MAX reaches the store: either the device should refuse it (EINVAL) before the store, as a real store would need, or the fake store should answer it as the real one does. Which is the question: check what crates/cladefs's store does with such an offset. The run's log was /mnt/fxdax-scratch/perf-linux/test.log on ubu1 (removed after).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A driver write whose offset plus length overflows is answered the way the real store answers it, and the test asserts that exact answer
- [ ] #2 whatever_a_driver_asks_is_answered passes on Linux with that case in its regressions
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Reproduce on macOS: force whatever_a_driver_asks_is_answered's offsets to u64::MAX - back; then a fixed regression unit test (WRITE at 0xffff_ffff_ffff_f000, 4096 bytes, and the like for READ, MAP, UNMAP, PRIVATE) red before the fix.
2. Real store and doc-4: extent_store's check_io refuses I/O unaligned to block_size, of no bytes, or ending past max_extent (overflow included) with EINVAL (Unaligned/TooLong); cladefs's window check_range refuses unaligned, empty, overflowing or past-the-window ranges with EINVAL; a MAP's extent offset unaligned or overflowing, EINVAL. doc-4: READ/WRITE offset and len multiples of block_size, end within max_extent; MAP/UNMAP/PRIVATE multiples of map_alignment, range within the window; EINVAL for an out-of-range offset, length or window offset.
3. Device guard: protocol::Bounds (block_size, max_extent, map_alignment, window length), made from the device's configuration space and window (Bounds::of), checked in requests::serve for READ, WRITE (Bounds::io), MAP (Bounds::mapped for its extent range, Bounds::window), MAP_ZEROS and MAP_MANY entries, UNMAP and PRIVATE (Bounds::window), before any store call; Requests and handle take it.
4. Fake store answers as the real one for what still reaches it (checked arithmetic, EINVAL as check_io / check_range do), and records any ask out of doc-4's ranges, which the tests assert empty; its lock ignores poisoning so a failed case reports its own failure.
5. Class test: a proptest of the range-bearing requests with edge offsets and lengths (aligned at and past each bound, unaligned, near u64::MAX, wrapping to 0) asserting the device refuses exactly what an independent oracle calls out of range, with EINVAL, the store unasked, and passes on the rest; whatever_a_driver_asks_is_answered's generator gains aligned values; daemon.rs's harness passes the bounds.
6. Prove: cargo nextest run -p virtio_extent, -p cladefs; PROPTEST_CASES raised; scripts/test.sh clippy; put the guard back out and see the tests go red.
7. File what else turned up (guest/sim/kernel/device.c's unchecked window arithmetic; the real store's zero-length/misaligned sealed reads).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
How found: ubu1's Linux workspace run (2026-11-15). Reproduced on macOS by forcing the proptest's offset to u64::MAX - back and its type to WRITE: the fake's write panicked (capacity overflow; 'attempt to add with overflow' with an end past 2^64), then 8,001 PoisonError failures, shrinking giving up, as on Linux.

Cause: the device passed every READ/WRITE/MAP/UNMAP/PRIVATE range to its store unchecked (only MAX_TRANSFER and room were checked), relying on each store's own checks; the fake had none and did unchecked usize arithmetic.

What the real store answers: extent_store's Store::check_io (begin_write, locate_entry) refuses offsets or lengths not multiples of a block, a length of 0, or an end past max_extent, overflow included, as Unaligned/TooLong, which cladefs maps to EINVAL; the window's check_range (map, map_zeros_at, unmap, make_private) refuses unaligned, empty, overflowing or past-the-window ranges with EINVAL; map's extent offset unaligned or overflowing, EINVAL. Exception: reads of sealed extents go through read_cached, which checks only the end (TASK-484). doc-4: READ/WRITE offset and len multiples of block_size, end within max_extent (WRITE) / the extent (READ); MAP/UNMAP/PRIVATE multiples of map_alignment, range within the window; Errors: EINVAL for a misaligned or out-of-range offset, length or window offset.

Fix: protocol::Bounds (block_size, max_extent, map_alignment, window length; Bounds::of the device's configuration space and window), checked in requests::serve before any store call: READ and WRITE (Bounds::io), MAP of an extent (Bounds::window and Bounds::mapped) and of zeros (Bounds::window), every MAP_MANY map before any is sent, UNMAP and PRIVATE (Bounds::window). handle and Requests::new take the bounds; Device::requests makes them. FREE, COMMIT, ROOT, INFO, FREEZE, STATS, RESIZED, ADVANCE, LET_GO, PIN, UNPIN carry no range. Zero lengths still reach the store, which answers them (doc-4 says nothing; TASK-484).

Fake: answers ranges as cladefs's store (check_io, check_range, map's offset check) with non-wrapping arithmetic, reads zeros past an extent's end, refuses another branch's writes EPERM, frees a missing extent successfully (it said ENOENT, unlike cladefs and doc-4: many_past_their_most_are_refused now asserts success, as daemon.rs's twin does); records every range doc-4 calls out of range (out_of_range), which the tests assert empty; its lock ignores poisoning, so a failed case reports its own failure and proptest shrinks.

Blind spot: the fake differed from the real store (no range checks at all), and the device relied on its store to refuse; the generator rarely made a WRITE that reached the store with an end past the fake's arithmetic, once in a Linux run. Class test: ranges_are_refused_exactly_past_their_bounds, a proptest of every range-bearing kind at edge offsets and lengths (at/below/past each bound, unaligned, near u64::MAX, aligned wrapping to 0) against an independent u128 oracle: refused EINVAL and the store unasked iff doc-4 calls it out of range, asked otherwise. whatever_a_driver_asks_is_answered gains block-multiple offsets and asserts nothing out of range reached the store; both honor PROPTEST_CASES (2000/4000 at least). Regression: ranges_past_their_ends_are_refused, red before the fix with 'attempt to add with overflow' at the fake's write. Mutants, each red on both new tests: WRITE's check out, PRIVATE's out (whatever_ red too), MAP's extent check out, within() wrapping_add, READ refusing everything.

Look around: guest/sim/kernel/device.c's window checks wrap and UNMAP/READ check nothing (TASK-483); sealed reads skip check_io (TASK-484). The C driver and Fxfs send only aligned ranges (max_io rounded to blocks; selftest sizes block/map aligned), so the guard refuses nothing a guest sends.

Structure: the range checks now have one owner on the device side (Bounds, in serve, before each store call). What could make the class impossible: Store's methods taking a range type only Bounds makes (a checked offset and length) instead of raw u64s, so a new request kind cannot reach a store unchecked; not done here, as Store's Linux impl (RemoteExtentStore) can't be compiled on this Mac; worth doing with a Linux build.

Results (macOS, 8811f315e): cargo nextest run -p virtio_extent 32/32 passed; PROPTEST_CASES=50000 on the three range tests passed (81 s, 6.7 s); cargo nextest run -p cladefs 365 passed, 9 skipped; CLADEFS_TEST_PARTS=clippy scripts/test.sh clean (fmt, clippy, shuttle, loom); scripts/upstream-changes.py --check passed. Not run: Linux. device.rs, Requests and tests/daemon.rs are Linux-only and uncompiled here (cross clippy for x86_64-unknown-linux-gnu fails on aws-lc-sys's missing x86_64-linux-gnu-gcc); AC #2 and DoD #1 wait for a ubu1 run.
<!-- SECTION:NOTES:END -->
