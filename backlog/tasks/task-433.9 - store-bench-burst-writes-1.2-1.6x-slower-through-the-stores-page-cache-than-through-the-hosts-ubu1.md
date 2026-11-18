---
id: TASK-433.9
title: >-
  store-bench burst writes 1.2-1.6x slower through the stores' page cache than
  through the host's (ubu1)
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-16 15:47'
updated_date: '2026-11-18 04:41'
labels: []
dependencies: []
references:
  - crates/extent_store/src/pages.rs
parent_task_id: TASK-433
priority: medium
type: bug
ordinal: 886000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Left by TASK-433.8, which fixed the burst commit (now at or under the host page cache's). store-bench burst on ubu1, fxjob --bench, medians rounds 2-6, 0ef6fa1b4's code (host page cache) vs main 3e0256500: a 256 KiB write 311 vs 368 us on ext4 over a RAM-backed loop device, 294 vs 467 us on ext4 on the scratch zvol (docs/measurements/ubu1-burst-page-cache-2026-11-16.md). Not page zeroing (pages a write fills whole made unzeroed bought nothing, 2ddfa1dcb, dropped). Candidates, unmeasured: write_runs copies a run (1 MiB) into its buffer under the file's pages lock, which a writer of the same file waits on; one write-back in flight at a time (QD1); a BTreeMap insert and a 4 KiB allocation per page.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A profile (perf, lock waits) names where a burst write's time goes beyond the host page cache's
- [ ] #2 Burst writes on ubu1 within 10% of 0ef6fa1b4's, an A/B under fxjob --bench, each side its own target
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Probe (scratch, both sides: per-step timers in Store::write_with, RawFile::write_at, Cached::write_at, write_runs) + perf, ubu1 fxjob --bench, medians rounds 2-6, 073feb370 vs 150b82934 (host page cache):
- store-bench burst timed making its 256 KiB of data with each write (~200 µs on a Mac): fixed, made before timing.
- 150b82934 predates TASK-411: its stores are clear; main encrypts every raw write. Per write (RAM / zvol): p 157 / 82 µs; main 467 / 477 µs, of which encryption 336 / 335 µs (AES-256-XTS a block at a time, storage_xts: TASK-512).
- main's copy into the stores' pages 88 / 91 µs; perf: almost all calloc of each 4 KiB page and the kernel's faults on that fresh memory (zeroing itself was cheap: 2ddfa1dcb bought nothing). The host's pwrite into its page cache took ~63-80 µs.
- Not lock waits: page lock 3-5 µs, throttle 0, the writer's wait for the writeback lock 8 µs; the writer writes back nothing itself; the flusher's pwrite 260-280 µs per 254 KiB, off the writer.
- p's write_behind (sync_file_range) 11-18 µs and write_back 52 µs (RAM) against main's 0.3 and 8.4.

TASK-512 made encryption 4.6x faster (334 -> 72 µs). Then, xts side, malloc's heap in transparent huge pages (GLIBC_TUNABLES=glibc.malloc.hugetlb=1) or not, fxjob --bench, medians rounds 2-6: copy into the stores' pages 81 -> 20 µs (RAM), 84 -> 22 µs (zvol); a write in all 237 -> 161 µs (RAM), 248 -> 177 µs (zvol). So the copy's cost is the faults on each fresh 4 KiB page. What is left: the writer's wait for write-back, 52-66 µs/write, the flusher's 1 MiB direct pwrites one at a time its bound (~1.0-1.3 GB/s).

Page frames (a7c198c95 on t3/433.9-frames: 2 MiB slabs, MADV_HUGEPAGE, freed frames kept within the cache's bound, counted as its memory, released first by eviction). A/B vs the XTS fix alone, fxjob --bench, medians rounds 2-6: copy into the pages 84 -> 25 µs (RAM), 86 -> 27 µs (zvol); a write 220 -> 186 µs (RAM), 260 -> 223 µs (zvol); a second run 161 / 216 µs.
The writer then waits on write-back, 53-107 µs/write: the flusher's 1 MiB direct pwrite takes ~950 µs wall (strace -w), 163 µs CPU; bpftrace off-CPU: 221 ms of the flusher's in blk_io_schedule under iomap_dio_rw, i.e. the device completing it. dd oflag=direct of noise, 1 MiB, alone: ~300 µs per MiB (3.4 GB/s) on both disks, 4 at once 4.8 GB/s. So in-process the requests are 3x slower than dd's, beside the framer's, the packer's and the logs' I/O.
Tried, dropped: begun ranges merged and 4 MiB chunks: RAM 161 -> 210 µs (worse), zvol 216 -> 176 µs (better); mixed, not kept.

Reviews (verifying-reviewer, three rounds) rejected releasing single 4 KiB frames (MADV_DONTNEED zeroes neighbours on 16/64 KiB-page hosts; khugepaged re-collapses) and counting free frames in bytes() (evict_to evicted every clean page without freeing memory; the_governor_lets_parts_held_go_before_tenants_pages failed on ubu1), and mmap'd slabs the test heap budgets did not see. Final: slabs from the global allocator, aligned 2 MiB, MADV_HUGEPAGE; freed whole once idle past 8 spare; accounting as main's. Final A/B (xts vs frames, fxjob --bench, medians rounds 2-6): write 210 -> 165 µs (RAM), 276 -> 208 µs (zvol); copy 83 -> 25, 87 -> 29; durable commit 33.0 -> 25.5 ms, 46.6 -> 38.0 ms. Against p (clear, host page cache): 142 / 85 µs; ours less its encryption 102 / 143 µs. Left: the writer's wait for write-back, 53-85 µs a write. Mac 2659/2659 + Shuttle 37/37; ubu1 workspace 2973/2973 + sim + Shuttle + loom; scripts' one failure TASK-500's flake.
<!-- SECTION:NOTES:END -->
