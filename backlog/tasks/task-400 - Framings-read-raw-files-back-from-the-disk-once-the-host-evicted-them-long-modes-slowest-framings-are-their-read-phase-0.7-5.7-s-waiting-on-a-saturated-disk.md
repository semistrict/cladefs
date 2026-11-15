---
id: TASK-400
title: >-
  Framings read raw files back from the disk once the host evicted them: long
  mode's slowest framings are their read phase, 0.7-5.7 s, waiting on a
  saturated disk
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-13 15:12'
updated_date: '2026-11-14 21:22'
labels:
  - perf
  - extentd
dependencies: []
priority: high
type: bug
ordinal: 783000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-336's instrumentation (read / compress / write phases, runnable waits). sandboxes long mode on ubu1 (fxjob --bench --mem 16G, its cgroup at its 16 GiB limit, reclaiming), main c6ba0d117 (2026-11-13): 19 framings past 1 s, the slowest of them their read phase, 0.7-5.7 s with 0-50 ms runnable: the segment's raw file, written by its guest moments before (and written back by write-behind, TASK-221), was evicted from the page cache under the cgroup's memory pressure and is read back from a disk eight VMs and RustFS keep busy. Main before TASK-336 in the same conditions (same afternoon) failed 11, the slowest 3.0 s, read-bound too. Ideas, unmeasured: frame (or start framing: checksum, sample, compress parts) while the segment's data is still in memory, part by part as it fills, so closing has only the tail left; or keep a closed segment's raw pages resident until framed; or size the long mode's cgroup and daemons' caches so the page cache keeps the last segments. Measure with the framing warnings' read phase and runnable.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Long mode on ubu1 under fxjob --bench: no framing's read phase past 0.5 s, over two passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Look at each part of a segment as it fills, once all its bytes are written (writes ended, appends), on the store's framer queue (Job::See), off the guests' writes: CRC + Look through the page cache; the framing takes the looks and reads only the parts not seen; a copy reads the rest.
2. Instrument: the framing warning says in place / copy, MiB and MiB read; extentd stats framing_read_bytes, seen_ahead_bytes; sandboxes summary.
3. Measure: short mode ABBA on ubu1 (fxjob --bench), GitHub short mode, two long-mode passes.
4. What's left after the long mode: copies (compressible segments) still read their data back at close.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-14 (claude): 14e7f8ae6 (look-ahead) with TASK-401's 1649d384e. Short mode on ubu1, base/fix/fix/base + look-only control: slow framings' read phase summed 3.01/2.66 s -> 0.42/0.55 s (look-only 0.57 s), longest read 157/121 -> 37/34 ms; framings read 2.1-2.2 of 5.0 GiB per daemon at close, 5.0 GiB seen ahead. GitHub short mode (3 runs on the fix): longest read 86-172 ms.
Long mode on ubu1 (fxjob --bench --mem 16G, 2 passes, fix artifacts 88fb47e9 = the pushed code but for an equivalent iterator rewrite): both FAILED the framing check, 2 a pass past 1 s, both on host-b. Seen ahead 42.0-42.3 GiB a daemon, framings in place ~470 a daemon, read at close 14.6-15.0 GiB: what's left is copies (compressible segments: pgbench, builders), whose framing still reads their raw data back, evicted: 96 MiB copy read 1,699 ms, 30.7 MiB copy read 3,458 ms, 96 MiB 1,589 ms; one in place 33.7 MiB read whole (packing tried) 587 ms. Pass 1 also had a 1 MiB copy whose write phase took 19,938 ms with no write-back wait (TASK-401 took those out): the disk stalled (host-b's durable lag max 25.1 s that pass), the framing's file create or write blocked with everything else. Read phase past 0.5 s: 1 (pass 1), 5 (pass 2). Against TASK-336's main c6ba0d117 long2 (19 past 1 s, slowest 5.77 s): 2 a pass, slowest 19.9 s (the stall) and 3.47 s. Durable lag within 50 ms 50.1/59.4% and 32.7/33.1%; max 6.2/25.1 s and 9.5/8.4 s; stores 1.20-1.21x; VMs 800 / 781 s.
AC #1 not met: copies. Next: frame copies as their segments fill too (pack each part as it is written whole, the packed bytes bounded per process, or written to the copy as it goes once the looks say it compresses), measured against the long mode.

2026-11-14, user's decision (copies' read-back, what's left of this task): a segment's parts are compressed as each is written whole, while in memory; the packed parts are held in memory under extentd's memory governor (TASK-358) as one more consumer of the daemon's memory, evicted before guests' pages; an evicted part is read back from the raw file and compressed at close, as today (per part, so a segment may keep some and lose others). No fixed cap; nothing written to disk early (writing parts into the copy as they come would write most segments' data twice, undoing TASK-310). Held parts are dropped once a segment's running look says it frames in place, and at a framing in place.
<!-- SECTION:NOTES:END -->
