---
id: TASK-395.18
title: >-
  A pass parses an object's key once an extent of every state it keeps, most of
  its CPU over a cell forked at many points
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 15:48'
updated_date: '2026-11-14 18:33'
labels:
  - collector
  - performance
dependencies: []
parent_task_id: TASK-395
priority: low
ordinal: 796000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found measuring TASK-395.12 (macOS, a file bucket, fxdax-gc-fill --vm 1024 100: a cell whose branch of 164k extents was forked at 100 points; fxdax-gc --cell 1/1). Live::behind_at parses each extent's location key (layout::kind) to learn whether it is moves, for every extent of every state a pass keeps (keep_point, inherit, add): 8M parses a pass for 100 fork points. sample: layout::in_cell, kind, hex and memchr were ~60% of the pass's CPU samples. The pass took 3.6 s, 0.26 s with one point.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A pass parses each object key it asks behind_at of once, not once an extent
- [x] #2 The 100-point pass of fxdax-gc-fill --vm 1024 100 measured before and after
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Live::behind_at memoizes each key's moves-ness (FxHashMap, rustc-hash, workspace dep).
2. Re-time the 100-point pass (fxdax-gc-fill --vm 1024 100, fxdax-gc --cell 1/1).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Profile (sample(1), main ec149e6de, 100-point pass): layout::in_cell 377, keep_point 353, hex 164, kind 164, memchr 146 samples of ~1,400 on-CPU: behind_at's key parsing ~60%. After the memo: keep_point 649, the memo's hash 245 with std SipHash, so FxHashMap. Pass: 3.50-3.75 s -> 2.74-2.97 s (with TASK-395.19). What remains is keep_point's map work and decoding, a cost an extent a fork point (TASK-395.12). Blind spot: none in tests (correctness unchanged; behind is the same set); measured, docs/measurements/macos-gc-fork-points-2026-11-14. Structure: fine; a Location could carry its key's kind, but keys are Strings shared through checkpoints' tables, and the memo is per pass.

macOS: cargo clippy --workspace --tests, cargo nextest run -p extent_remote 307/307, scripts/test.sh (all parts; firecracker.lock updated for rustc-hash); Linux cross-check compiles; ubu1 run t39512h at 8eadcf0c2: workspace 2652/2652, guest tests pass, scripts' tests 8 failures all TASK-410 (main's TMPDIR, fixed after, test_host 60/60); upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Live::behind_at remembers each key's kind (FxHashMap) instead of parsing it once an extent; with TASK-395.19 the 100-point pass went 3.5-3.75 s -> 2.7-3.0 s (docs/measurements/macos-gc-fork-points-2026-11-14).
<!-- SECTION:FINAL_SUMMARY:END -->
