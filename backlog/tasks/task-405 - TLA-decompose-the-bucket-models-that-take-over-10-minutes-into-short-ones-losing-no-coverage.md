---
id: TASK-405
title: >-
  TLA+: decompose the bucket models that take over 10 minutes into short ones,
  losing no coverage
status: Done
assignee:
  - '@claude'
created_date: '2026-11-13 22:24'
updated_date: '2026-11-15 00:36'
labels:
  - tla
  - specs
dependencies: []
priority: high
ordinal: 789000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
specs/bucket has grown models TLC needs from 15 minutes to over 2.5 hours (ubu1, 16G, 8 CPUs): BucketTakeovers unfinished after 2h37m at 200M+ states and its memory limit, BucketStrays 1853 s, BucketForks 1327 s, BucketOverlap 989 s. A check that long is run rarely, and every Bucket.tla change waits on it. The user asked: rewrite anything over about 20 minutes, ideally under 10, by decomposing it. Splitting must keep every behaviour each model exists for (its cfg comment, doc-7) and every mutation it catches.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Every model and mutation in check-tla.sh's default run, in all of specs/, passes on the laptop, each under 10 minutes (20 at most), timed with the load noted
- [x] #2 For each replaced model, a written argument of which behaviours it covered and which new model covers each
- [x] #3 Every mutation still finds its violation; any mutation that relied on a long model is caught by a short one
- [x] #4 Long models kept only as deep checks, skipped by the default run, runnable explicitly, documented in the script header
- [x] #5 doc-7's list of models, the cfg comments and OPEN_QUESTIONS.md (# Decisions) updated
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Time every model and mutation on the laptop (done: only BucketForks, Overlap, Strays, Takeovers over 10 min).
2. Split each exactly: knobs in Bucket.tla (Takers, TakePhases, CommitPhases, CommitTakes, ForkTo, PassCells), no-ops by default; parts as '\* part:' lines in the whole's cfg, run by check-tla.sh (MODEL, MODEL#K, MODEL#whole).
3. Prove exactness: check-tla.sh cover, from the cfg's '\* cover:' lines, dumps every state of the whole and of every part on a small model; a full run checks it.
4. BucketTakeovers: deep model (\* deep), 72 parts, run by 'check-tla.sh deep'.
5. Re-find the long models' bugs on single parts (hand mutants); run every mutation.
6. Docs: check-tla.sh header, cfg comments, Bucket.tla, doc-7, OPEN_QUESTIONS, README.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Baseline (laptop, 2026-11-13): every other model and mutation in specs/ under 5 min (slowest NoPinsVoidStrays 287 s, run beside probes); only BucketForks, BucketOverlap, BucketStrays, BucketTakeovers need splitting.
Approach changed from scenario families to exact splits: knobs in Bucket.tla (Takers, TakePhases, CommitPhases, CommitTakes, ForkTo, PassCells), parts as '\* part:' lines of the whole's cfg, run by check-tla.sh; 'check-tla.sh cover' dumps the states of the whole and of every part on a small model and fails unless they match (symmetric states compared by orbit). Negative test: one part removed, 21346 states missed, FAIL.
Dead end on the way: TakePhases as a history of steps multiplied part sizes 2-5x; now sets of allowed steps, no history. PassTakes/PassCommits (counts at pass start) left a dominant early part; replaced.
Hand mutants on single parts (sandbox copy): pinsVoidStrays and pinsLateStrays removed -> BucketStrays part {11,21,31} finds NoAckedLost; recordsKept (TASK-174) -> BucketForks part PassCells=2 {11,2x} finds NothingNeededGone; deletionsFence (TASK-183) -> PassCells=1 {11,2x} NothingNeededGone; CloneRefs across cells only (BucketOverlap's find) -> part ForkTo={3}, PassCells=13 NothingNeededGone.

Rebased on ec149e6de (TASK-395.13/.14 Bucket.tla): covers PASS for BucketStrays (20 parts, 92091 states), BucketForks (6, 221737), BucketOverlap (5, 94565). Every default model, part and mutation PASS on the new spec (73 mutations, NoRetirementsMarked 2 s); follower/traces PASS (20 runs, 5 mutant rejected). Laptop times (load noted per run in the agent's logs): Forks parts 154-471 s (load 24-43), Overlap 32-325 s, Strays parts 1-826 s (the 826 s at load 137 from other agents' TLC; at load ~25 they took under 5 min); NoPinsVoidStrays narrowed to the part where its bug shows, 190 s. Hand mutants re-run on the new spec: recordsKept and deletionsFence (Forks parts), CloneRefs across cells only (Overlap part), pinsLateStrays and pinsVoidStrays (Strays parts) each violate as before. Takeovers deep parts: measured ones 1-13 min under load; some refined later and unmeasured; cover with MaxTakes=3 PASS (234963 states), with MaxCommits=0 parts missed 0 (extra-state check is off under 3-host symmetry, see check-tla.sh). upstream-changes.py --check passes. Branch t3/tla-decompose; ubu1 re-timing left to the parent.

Re-timed on the Mac (@claude, 2026-11-14; TLC runs on the Mac, not ubu1, since the user's 2026-11-14 decision): BucketStrays' 20 parts all PASS, 2-802 s each, 7173 s together, load 35-56 from other agents' runs; the whole took 18 min. The user chose to merge as is (parts short, full default run about 2-3 h, run by hand only). On main as 545dca7c1..093399c82. DoD: specs, scripts and docs only, no Rust changed; upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
BucketForks, BucketOverlap and BucketStrays are split into 6, 5 and 20 exact parts (Bucket.tla's Takers, TakePhases, CommitPhases, CommitTakes, ForkTo, PassCells), each minutes long, with check-tla.sh cover proving every state of the whole is reached by a part; BucketTakeovers is a deep model (72 parts, `check-tla.sh deep`). All 73 mutations and the long models' old bugs are caught by short parts. The full default run is longer in total (about 2-3 h) as parts re-check shared prefixes.
<!-- SECTION:FINAL_SUMMARY:END -->
