---
id: TASK-395.17
title: >-
  fxdax-gc's sweep at 4 passes at once took 559 s on ubu1, against 320 s before
  TASK-395.15
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 03:12'
updated_date: '2026-11-14 18:33'
labels:
  - collector
  - performance
dependencies: []
parent_task_id: TASK-395
priority: low
ordinal: 791000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ubu1 run t395sweep2 (main 79c8249e1, docs/measurements/ubu1-gc-sweep-2026-11-14T0233Z-e344b176): first sweeps 602/559/190/112 s at 1/4/16/64 at once, against 629/320/492/395 s on c9b5ba549 (ubu1-gc-sweep-2026-11-13T2259Z-50e7eb9e). 16 and 64 got 2.6x and 3.5x faster, but 4 got 1.75x slower; one run each, so it may be noise. Re-measure 4 at once a few times on both commits; if it holds, find why (the multi-thread runtime's worker count, the 64 lock stripes, blocking-pool hand-offs).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 4 at once measured at least 3 times on each of c9b5ba549 and the current main, with the spread
- [x] #2 If slower beyond the spread, the cause named and fixed
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/gc-sweep.sh: sides (SIDE=TREE, own targets and buckets), rounds with the sides' order turned each round, FXDAX_GC_SWEEPS=1 for first sweeps alone, a median/least/most table; gc-sweep-host.sh takes SIDE=COMMITs.
2. ubu1, one fxjob --bench job: old=c9b5ba549 new=main, 4 at once, 4 rounds, first sweeps only.
3. If new is slower beyond the spread: profile (workers, lock stripes, blocking pool), fix, re-measure.
4. Record in docs/measurements.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
ubu1 t39517a (one fxjob --bench job, 8 CPUs, sides built in own targets, own fills, 4 rounds turning order): old c9b5ba549 544.4/600.5/534.6/505.1 s (median 539.5), new ec149e6de 474.2/484.8/507.9/524.7 s (median 496.4). Not slower beyond the spread: no cause to fix (AC #2 holds vacuously). The sweep is bound by ubu1's scratch disk (ext4 on a ZFS zvol: ~11% iowait, jbd2/zvol threads busy, fxdax-gc ~46% of a core), so single runs swing; the 320 s was one run. Measurement: docs/measurements/ubu1-gc-sweep-ab-2026-11-14-t39517a. macOS: cargo clippy --workspace --tests, cargo nextest run -p extent_remote 307/307, scripts/test.sh (all parts; firecracker.lock updated for rustc-hash); Linux cross-check compiles; ubu1 run t39512h at 8eadcf0c2: workspace 2652/2652, guest tests pass, scripts' tests 8 failures all TASK-410 (main's TMPDIR, fixed after, test_host 60/60); upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
4 passes at once measured 4 times on each commit, interleaved, in one ubu1 bench job (scripts/gc-sweep.sh's new sides, rounds and FXDAX_GC_SWEEPS=1): main 496 s median (474-525) against c9b5ba549's 540 s (505-601). No slowdown; the 559/320 s pair were single runs of a disk-bound sweep. Recorded in docs/measurements and OPEN_QUESTIONS.
<!-- SECTION:FINAL_SUMMARY:END -->
