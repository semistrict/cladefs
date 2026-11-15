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
updated_date: '2026-11-15 05:07'
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

5. (user's decision, copies) Refactor first: extent_frame Packer::push_stored (a part's stored bytes and length, not its data); framing::Read takes a part packed ahead; a framing takes its segment's looks as one Looked per part.

6. extent_store::held: HeldParts, a ledger of parts packed ahead across the stores sharing it (bytes, peak, evictions; evicted newest first), each part a Hold the segment keeps; a store takes it in Opening / upload::Options beside the host's cache (a store-local ledger by default, nothing evicting it).

7. The framer's look ahead packs a part (kept packers) when its segment's running look, every part seen so far, doesn't say it frames in place; holds it; drops the segment's held parts when the look says in place, and at a framing in place. A framing takes the held parts, packs and reads only the rest; audit: a held part is the part packed again.

8. extentd: the governor counts held parts in what the daemon holds and evicts them before any tenant's pages; stats held_parts_*; sandboxes summary.

9. Prove: framing/held unit tests (held, evicted, partial, framed bytes identical), store tests, governor test; crash tests and sim; store-bench framing; ubu1 short mode base/fix; long mode two passes (ask first).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-14 (claude): 14e7f8ae6 (look-ahead) with TASK-401's 1649d384e. Short mode on ubu1, base/fix/fix/base + look-only control: slow framings' read phase summed 3.01/2.66 s -> 0.42/0.55 s (look-only 0.57 s), longest read 157/121 -> 37/34 ms; framings read 2.1-2.2 of 5.0 GiB per daemon at close, 5.0 GiB seen ahead. GitHub short mode (3 runs on the fix): longest read 86-172 ms.
Long mode on ubu1 (fxjob --bench --mem 16G, 2 passes, fix artifacts 88fb47e9 = the pushed code but for an equivalent iterator rewrite): both FAILED the framing check, 2 a pass past 1 s, both on host-b. Seen ahead 42.0-42.3 GiB a daemon, framings in place ~470 a daemon, read at close 14.6-15.0 GiB: what's left is copies (compressible segments: pgbench, builders), whose framing still reads their raw data back, evicted: 96 MiB copy read 1,699 ms, 30.7 MiB copy read 3,458 ms, 96 MiB 1,589 ms; one in place 33.7 MiB read whole (packing tried) 587 ms. Pass 1 also had a 1 MiB copy whose write phase took 19,938 ms with no write-back wait (TASK-401 took those out): the disk stalled (host-b's durable lag max 25.1 s that pass), the framing's file create or write blocked with everything else. Read phase past 0.5 s: 1 (pass 1), 5 (pass 2). Against TASK-336's main c6ba0d117 long2 (19 past 1 s, slowest 5.77 s): 2 a pass, slowest 19.9 s (the stall) and 3.47 s. Durable lag within 50 ms 50.1/59.4% and 32.7/33.1%; max 6.2/25.1 s and 9.5/8.4 s; stores 1.20-1.21x; VMs 800 / 781 s.
AC #1 not met: copies. Next: frame copies as their segments fill too (pack each part as it is written whole, the packed bytes bounded per process, or written to the copy as it goes once the looks say it compresses), measured against the long mode.

2026-11-14, user's decision (copies' read-back, what's left of this task): a segment's parts are compressed as each is written whole, while in memory; the packed parts are held in memory under extentd's memory governor (TASK-358) as one more consumer of the daemon's memory, evicted before guests' pages; an evicted part is read back from the raw file and compressed at close, as today (per part, so a segment may keep some and lose others). No fixed cap; nothing written to disk early (writing parts into the copy as they come would write most segments' data twice, undoing TASK-310). Held parts are dropped once a segment's running look says it frames in place, and at a framing in place.

2026-11-15 (claude, held parts, WIP c0987eee unpushed): packed parts held under extentd's governor (extent_store::held::HeldParts, taken by stores in Opening/upload::Options; extentd's memory::held_parts, govern evicts them first; anon budget counts their peak). Looks ahead batched (framing::see_parts: read+look in order on the framer, packing on threads together): packing each part alone on the framer fell behind (store-bench 64 segments 10-15 s vs 2.5 s). Tests: Mac extent_store+extent_frame 296/296, clippy, Shuttle; ubu1 extentd governor/stats tests. store-bench framing on tmpfs (ubu1 bench, BFFB): avg 41.5/41.2 -> 17.3/15.9 ms, wall and CPU unchanged; 28 spinners avg 136/137 -> 68/64 ms. Sandboxes short mode on ubu1 (bench), 6 runs base-r1, fix-r1, fix-r2, base-r2, base-r3, fix-r3: all PASSED; slow framings 20/16, 16/13, 11/14 (base) -> 2/4, 4/4, 3/4 (fix); slowest 402/269, 187/107, 130/155 -> 265/231, 173/221, 92/103 ms; read phase summed 489/254, 268/231, 141/233 -> 28/62, 31/37, 29/32 ms; framings read 2.1-2.3 -> 0.14-0.26 GiB a daemon; held 0.64-0.72 GiB, taken 0.27-0.33 (half dropped, segments framed in place), peak 32-78 MiB, 0 evicted; daemon CPU per guest MiB 9.03, 8.38, 8.07 -> 8.10, 8.50, 8.07 ms. Durable lag within 50 ms (host-a/b): base 69/68 (load 4.5), 25/24 (load 28), 33/34 (load 32); fix 34/35 (load 9.6), 40/38 (load 4.3), 15/16 (load 30); max base 1.4-6.5 s, fix 6.0-9.2 s. Leans to a regression from the look ahead's packing threads (nice 0, up to the CPUs, while guests write and commit); not settled. Long mode not run.

2026-11-15 (claude): variant (WIP 66bb6cf8, unpushed): packing ahead off the framer, on 2 process-wide threads at SCHED_IDLE (nice 19 fallback), async: parts held pending until packed; a framing finding one pending or skipped (queue past 64) reads and packs it itself at its own priority, the late result let go: no framing waits on the look ahead. Mac extent_store+extent_frame 297/297, clippy, Shuttle green. Short mode ubu1 bench, one job v/base/base/v, nothing else running beside it (fxjob ps: only queued t417-check, t411): all PASSED. Durable lag within 50 ms a/b: variant 47/41, 40/38; base 43/45, 40/34; p50 64 ms all but base-r5 b 128; max variant 4.2-5.1 s, base 3.8-5.8 s: no regression now. Slow framings v 9/11, 3/3 vs base 13/16, 13/19; read phase summed v 51/85, 38/64 vs base 251/275, 173/245 ms; slowest v 172/281, 155/511 vs base 324/133, 170/216; framings read 0.22-0.29 vs 2.15-2.26 GiB a daemon; held 0.61-0.63 GiB, taken 0.28-0.36, peak 36-48 MiB, late 57-98 parts a host, 0 skipped, 0 evicted; daemon CPU per guest MiB v 8.86/8.72 vs base 8.76/8.56 ms. Load at round start 3.7 (first), 25-34 after (the previous round's own tail).

2026-11-15 (claude): pushed to main as c2daf7630 (rebased on df59917ba; the variant: packing ahead on 2 SCHED_IDLE threads, async). Mac on the rebased tree: extent_store+extent_frame 298/298, clippy, Shuttle (one LEAK flag, not this: TASK-420). Next: main's CI (43666377909), then two long-mode passes on main under fxjob --bench (AC #1).

2026-11-15 (claude): main's CI on c2daf7630's push (43666377909): every job passed but test (clippy scripts): crates/fxdax/mutants/framed-in-copies.patch didn't apply to the new frame_segment; re-anchored (same mutation) in b4ac3d970, whose CI (43668192071) passed clippy scripts (the rest running when last looked at).
Long mode (AC #1) NOT RUN: ubu1's /mnt/fxdax-scratch had 48 of 251 GB free (another session's /mnt/fxdax-scratch/t411, 165 GB, and its t417f job running); a long pass writes ~100 GB (its bucket ~90 GiB). Left ready on ubu1: main's artifacts at 4f2a634d (TASK-400's code, c2daf7630) in ~/src/fxdax-t400-main-artifacts; guest kernel ~/src/t400-kernel/out/x86_64/vmlinux; worktree ~/src/fxdax-t400-fix (detached at 4f2a634d), whose scripts/qualify.sh the passes run; scripts /mnt/fxdax-scratch/t400-long/pass.sh N (qualify.sh, FXDAX_QUALIFY_SANDBOXES=long, FXDAX_QUALIFY_SOAK_SECS=0, as root with fxjob's TMPDIR, 20 min hard timeout, results /mnt/fxdax-scratch/t400-long/passN, log passN.log) and both.sh (passes 1 and 2 back to back, each with load, Dirty, io pressure and fxjob ps noted first). Start, once 110+ GB is free and nothing else runs (fxjob ps): ssh ubu1 'PATH=$HOME/.cargo/bin:$PATH fxjob --bench --name t400-long --mem 16G -- /mnt/fxdax-scratch/t400-long/both.sh'. Read: each pass's sandboxes summary (results/passN/sandboxes: 'sandboxes: host-X: slow framings' phases', 'a framing of', held_parts_*), the framing warnings' read phase (none past 0.5 s for AC #1), the framing check (none past 1 s: TASK-336, TASK-330.5), bench-budgets.json's cpu_per_guest_mib, durable lag from host-X-stats.txt's daemon: line. Then remove ~/src/fxdax-t400-fix, ~/src/fxdax-t400-main-artifacts and /mnt/fxdax-scratch/t400-long (keep t400-kernel while guest/linux is unchanged).

Coordinator 2026-11-15, the long mode: Long mode on ubu1, main's code at 4f2a634d (TASK-400 c2daf7630, TASK-401 9c93d8d08, TASK-336 c6ba0d117), fxjob --bench --mem 16G, 2026-11-15 04:35-05:05 UTC, two passes back to back (scripts/qualify.sh, FXDAX_QUALIFY_SANDBOXES=long): both PASSED in ~15 min each (VMs ran 858 s). Framings over 1 s: 0 on every host of both passes (before: 2-19 a pass). Slowest framing host-a/b: pass 1 703/691 ms, pass 2 784/935 ms; slow framings (over 50 ms) 107/104 and 103/117 a host. Read phase: one framing past 0.5 s in all (pass 1 host-a, in place, 16.8 MiB read of 34.8 MiB, its parts packed too late: read 622 ms); maxima 622/200 and 130/343 ms. Held parts peak 71-117 MiB, 0 evicted; framing reads 2.45-2.69 GiB a daemon against 41.5-42.7 GiB looked at ahead. Stores wrote 1.20x the guests; budgets: cpu_per_guest_mib 13.9 ms (budget 20), max_anon 1,060 MiB, io_per_guest_byte 1.207. Host before pass 1: load 5.4, Dirty 453 MB, io some avg60 14%; before pass 2 load 51 (pass 1's VMs ending), io 36%; another session's e2e411 job queued, not running. Results /mnt/fxdax-scratch/t400-long/pass1, pass2. AC #1 (no read phase past 0.5 s over two passes) NOT met by one framing of 431 (622 ms, an in-place framing whose parts the idle-priority look-ahead packed too late, 1,317-1,580 parts a host 'late'). Regression filed: TASK-423 (the look-ahead's reads land in extents_written_in_runs_seal_where_they_are's I/O window, main's CI red). ubu1 scratch kept for now: ~/src/fxdax-t400-fix, ~/src/fxdax-t400-main-artifacts, /mnt/fxdax-scratch/t400-long.
<!-- SECTION:NOTES:END -->
