---
id: TASK-433.8
title: >-
  store-bench burst regressed since the stores' own page cache: writes 2.3x, its
  commit's flush 5x slower on ubu1
status: Done
assignee:
  - '@claude'
created_date: '2026-11-16 02:12'
updated_date: '2026-11-16 16:02'
labels: []
dependencies: []
references:
  - crates/extent_store/src/pages.rs
  - crates/cladefs-examples/examples/store-bench.rs
parent_task_id: TASK-433
priority: high
type: bug
ordinal: 877000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-433.7's A/B (docs/measurements/ubu1-log-appends-ab-2026-11-15.md), one fxjob --bench job on ubu1, ext4 on /mnt/fxdax-scratch, medians of rounds 2-4: store-bench burst (1024 writes of 256 KiB, then one commit) at 0ef6fa1b4 (TASK-433.2) wrote 297 us a write and flushed its commit in 46.6 ms; at c78d0dbf2 (main: TASK-433.1/433.3/433.5, extent_store::pages under every Dir) 690 us and 235 ms (ranges 641-720 us, 118-243 ms). durable (2 extents a commit) is unchanged (6.3 vs 5.4 ms). Unconfirmed cause; candidates: the writer throttled at pages' hard threshold, writing back on its own thread; the commit's write_back_all writing back 256 MiB of dirty pages in 1 MiB runs one after another rather than in parallel; framing reading raw files back past the cache. Other commits between the two sides could matter too: bisect on ubu1 first.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The cause named with evidence (a bisect between 0ef6fa1b4 and c78d0dbf2, and a profile or the cache's usage counters)
- [ ] #2 burst's writes and its commit's flush on ubu1 back within 10% of 0ef6fa1b4's, an A/B under fxjob --bench, each side its own target
- [x] #3 durable unchanged (within its spread)
- [x] #4 Revised with the user: the burst commit's flush at or under 0ef6fa1b4's on ubu1 (A/B under fxjob --bench, each side its own target); the writes' remaining gap measured and filed
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Find the cause on ubu1 (bisect, RAM-backed ext4 to remove disk drift, strace, perf).
2. Fix the first cause: Cached::write_behind begins write-back through the flusher (f6eb96991).
3. A/B p (0ef6fa1b4's code) / m (main) / f (fix) on zvol and RAM ext4.
4. Remaining gap: decided with the user.

5. (user: cap from governor) PageCache::size_to(room): background room/10, hard room/5, most room, none below the defaults; extentd memory::govern sizes it each look; store-bench governs once on Linux as the daemon's first look (3e0256500, on main).
6. Pages a write fills whole made of its bytes, not zeroed first (2ddfa1dcb).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Evidence (ubu1, fxjob --bench): bisect puts it at 3139c4bdd (433.1/433.3), not 433.5. The scratch zvol drifts 4x within a run (dd direct 939 -> 208 MB/s), so the decisive runs are on ext4 over a RAM-backed loop device (/dev/shm), where it persists: commit 37 ms (p) vs 75-190 ms (q/m). Not write amplification: both sides write 260 MiB to the device; m's 274 1-MiB pwrites take 34 ms in all (strace --seccomp-bpf). m reads 87 MiB back from the device for framing (evicted past most = 256 MiB), p 0.7 MiB. perf --call-graph dwarf, main thread: CachedFile::write_back 11% on the writer thread, the flusher idle (0.4% CPU): Cached::write_behind only woke the flusher, which writes nothing under background (32 MiB) or before expiry (1 s), so the store's waiting write_back wrote each range itself, a MiB at a time; page faults 26%, memset 16% (alloc_zeroed bounce buffers), memmove 19%. Fix f6eb96991 (branch t4338): the flusher begins ranges written behind first, as SYNC_FILE_RANGE_WRITE. A/B medians rounds 2-6, p / m / f: zvol burst commit 59 / 391 / 142 ms, write 336 / 922 / 989 us; RAM burst commit 37 / 201 / 145 ms, write 513 / 862 / 754 us; RAM durable 1.57 / 0.70 / 0.66 ms. Gap left: writes ~2x, commit ~4x.

Second A/B (ubu1, fxjob --bench, quieter host), medians rounds 2-6, p (0ef6fa1b4's code) / f (write-behind fix, on main b49554ca2) / g (+ reused aligned buffers, 2e0952e1e): RAM ext4 burst write 287 / 405 / 371 us, commit 56.0 / 78.5 / 67.5 ms, durable 0.77 / 0.36 / 0.34 ms; zvol burst write 335 / 486 / 534 us, commit 51.4 / 110.8 / 113.5 ms, durable 6.8 / 8.3 / 5.4 ms. g's reuse test is mutant-checked (zero-fill removed: fails). Left: commit ~1.2x (RAM) to 2.2x (zvol), writes 1.3-1.6x; what remains is framing re-reading what eviction took (87 MiB past most = 256 MiB) and one write in flight at a time.

Third A/B (ubu1, fxjob --bench), medians rounds 2-6, p (0ef6fa1b4's code) / g (main before sizing) / h (governor-sized, 3e0256500): zvol burst commit 51.8 / 152.2 / 45.7 ms, write 304 / 498 / 494 us, durable 5.9 / 5.8 / 5.4 ms; RAM burst commit 77.9 / 108.5 / 33.1 ms, write 319 / 431 / 376 us, durable 0.85 / 0.39 / 0.44 ms. The commit is at or under the host page cache's now; writes 1.2-1.6x. cladefs extentd::tests::the_stores_page_cache_is_sized_to_what_the_governor_leaves passes on ubu1 (with 23 governor/memory tests). Mac suites green at each step (2549-2550 workspace, Shuttle 37/37).

Fourth A/B, p / h (main, governor-sized) / i (whole pages unzeroed, 2ddfa1dcb): RAM write 311 / 368 / 371 us, commit 94.0 / 32.9 / 31.8 ms; zvol write 294 / 467 / 572 us, commit 48.7 / 47.8 / 56.9 ms. i bought nothing measurable: dropped, never pushed. Writes stay 1.2x (RAM) to 1.6x (zvol) the host page cache's; commit at or under it.

AC #2 not met as written and left unchecked: superseded by #4 (decided with the user, 2026-11-16): commit met, the writes' 1.2-1.6x gap filed as TASK-433.9. AC #3: durable not slower, faster (RAM 0.84 -> 0.42 ms, zvol 6.8 -> 5.5 ms). DoD: macOS scripts/test.sh green at 3e0256500 (workspace 2550/2550, Shuttle 37/37, mutants clean); ubu1 under fxjob at 3e0256500: workspace 2861/2861, scripts part OK; upstream-changes.py --check passes; no upstream code touched. Results: docs/measurements/ubu1-burst-page-cache-2026-11-16.md.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
store-bench burst's commit regressed 3-5x on ubu1 with the stores' own page cache (3139c4bdd). Causes, each found with strace/perf on a RAM-backed ext4 that takes the disk's drift out: Cached::write_behind only woke the flusher, so the writer wrote back every range it waited for (fixed: the flusher begins ranges written behind first, f6eb96991); each write-back allocated and zeroed two fresh MiB (fixed: reused block-aligned buffers written as they are, 2e0952e1e); a fixed 256 MiB cap evicted a burst as it was written, to be read back for framing (fixed with the user: the cache sized to what the governor leaves it, 3e0256500). Burst commit now 47.8 vs 48.7 ms (zvol) and 32.9 vs 94.0 ms (RAM) against the host page cache; durable ~2x faster. Writes stay 1.2-1.6x: TASK-433.9.
<!-- SECTION:FINAL_SUMMARY:END -->
