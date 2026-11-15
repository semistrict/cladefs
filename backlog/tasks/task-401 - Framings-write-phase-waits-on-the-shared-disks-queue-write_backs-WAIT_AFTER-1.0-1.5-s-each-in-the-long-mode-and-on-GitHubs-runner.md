---
id: TASK-401
title: >-
  Framings' write phase waits on the shared disk's queue: write_back's
  WAIT_AFTER 1.0-1.5 s each in the long mode and on GitHub's runner
status: Done
assignee:
  - '@claude'
created_date: '2026-11-13 15:12'
updated_date: '2026-11-15 05:07'
labels:
  - perf
  - extentd
dependencies: []
priority: high
type: bug
ordinal: 784000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-336's instrumentation. With the framer's CPU work cut (TASK-336), the framings still past 1 s on GitHub's 4-CPU runner (sandboxes short mode, run 43435575870 on main 487f443a8: 1 a host, 1,085 and 1,549 ms) and many of ubu1's long mode's (c6ba0d117: 1.0-1.5 s) are their write phase with almost no runnable time: segments::write_back writes each MiB of a copy, or an in-place framing's tail, and waits for it to reach the disk (sync_file_range WAIT_BEFORE|WRITE|WAIT_AFTER, TASK-165.9) behind a queue the VMs, the flushes and RustFS keep full; one of 1,532 ms wrote a tail of a few KiB. Waiting paced framings so that commits' syncs didn't wait behind a framed segment's 32 MiB (GCE r728), when every segment was framed in a copy; now most are framed in place (tail only) and copies are ~6% of the raw bytes (short mode: framed 0.26-0.33 GB against raw 5.0 GB). Ideas, to measure against durable lag (doc-12) and the framing check: write a tail without waiting (the making durable after syncs it); begin a copy's write-back without waiting and let segments::make_durable wait; an I/O priority for framings' writes below flushes'.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 GitHub's sandboxes short mode: no framing's write phase past 1 s over several runs, durable lag no worse
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. A framing writes its copy, or its tail, to the page cache and starts no writing back.
2. make_durable writes each framed file back a MiB at a time, waiting for each, before its sync (the r728 pacing kept, off the framing).
3. The uploader reads framed files through the page cache (read_object).
4. Measure: ubu1 short-mode ABBA for write phase and durable lag; GitHub short mode, several runs.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-14 (claude): 1649d384e. First tried: a copy's writing back begun as written, waited for only 8 MiB behind (as raw files, TASK-221): one ubu1 short pair had durable lag within 50 ms 48% against 80% (journal commits wait for begun-unwaited ordered data), and GitHub still a 1.58 s write phase on a 75 MiB copy; dropped. Final, ubu1 short mode ABBA (fxjob --bench): write phase summed 4.59/6.12 s -> 0.37/0.62 s (look-only control 3.27 s), longest 644/867 -> 138/347 ms, slow framings 82/83 -> 29/33; within 50 ms base 66.1/28.1% fix 61.0/40.9% (the disk drifting through the 9 min, both sides). GitHub's runner, short mode: main (43627356560, 43633699019) failed, 3 framings past 1 s each, all write phase, longest 2,704/2,631 ms, within 50 ms 63.4/58.4 and 55.5/53.4%, max lag 3.5/2.1 and 0.8/10.4 s; fix (43631859938, 43633697009, 43633701029) all three PASSED, longest write phase 73/158/55 ms, write phases summed 0.40/0.54/0.34 s, within 50 ms 79.1/76.5, 80.9/74.6, 68.4/68.1%, max lag 1.6-2.3 s. Long mode (ubu1, 2 passes): longest write phase 594 ms but one 19.9 s, a 1 MiB copy's create/write during a disk stall (lag max 25 s that pass), no write-back wait in it. Linux workspace suite under fxjob at dddfea15e: 2653/2655, the 2 failures not this (TASK-309's ZFS target; TASK-328's flake). macOS workspace: 2372/2373, frees_make_room hung (filed TASK-414: a collector pass, no framing involved).

Coordinator 2026-11-15: AC #1 evidence (from the TASK-400/401 agent's runs): GitHub's sandboxes short mode on the fix passed 3 of 3 runs, longest write phase 73/158/55 ms, durable within 50 ms 68-81% (main: 2 runs failed, 3 framings past 1 s each, all write phase up to 2.7 s, within 50 ms 53-63%). Long mode on ubu1 also clean twice: Long mode on ubu1, main's code at 4f2a634d (TASK-400 c2daf7630, TASK-401 9c93d8d08, TASK-336 c6ba0d117), fxjob --bench --mem 16G, 2026-11-15 04:35-05:05 UTC, two passes back to back (scripts/qualify.sh, FXDAX_QUALIFY_SANDBOXES=long): both PASSED in ~15 min each (VMs ran 858 s). Framings over 1 s: 0 on every host of both passes (before: 2-19 a pass). Slowest framing host-a/b: pass 1 703/691 ms, pass 2 784/935 ms; slow framings (over 50 ms) 107/104 and 103/117 a host. Read phase: one framing past 0.5 s in all (pass 1 host-a, in place, 16.8 MiB read of 34.8 MiB, its parts packed too late: read 622 ms); maxima 622/200 and 130/343 ms. Held parts peak 71-117 MiB, 0 evicted; framing reads 2.45-2.69 GiB a daemon against 41.5-42.7 GiB looked at ahead. Stores wrote 1.20x the guests; budgets: cpu_per_guest_mib 13.9 ms (budget 20), max_anon 1,060 MiB, io_per_guest_byte 1.207. Host before pass 1: load 5.4, Dirty 453 MB, io some avg60 14%; before pass 2 load 51 (pass 1's VMs ending), io 36%; another session's e2e411 job queued, not running. Results /mnt/fxdax-scratch/t400-long/pass1, pass2.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Framings don't wait for their writes to reach the disk: the making durable writes framed files back a MiB at a time before its sync, keeping r728's pacing off the framer, and uploads read framed files through the cache (9c93d8d08). Verified on GitHub's short mode (3/3 passed, write phase at most 158 ms vs 2.7 s on main, durable lag better) and ubu1's long mode (two passes, 0 framings over 1 s). Main's CI green on Linux and macOS workspace with these changes (b4ac3d970, run 43668192071); upstream-changes.py --check passes (2026-11-15).
<!-- SECTION:FINAL_SUMMARY:END -->
