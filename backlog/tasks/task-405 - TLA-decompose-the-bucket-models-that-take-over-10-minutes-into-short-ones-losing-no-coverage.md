---
id: TASK-405
title: >-
  TLA+: decompose the bucket models that take over 10 minutes into short ones,
  losing no coverage
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-13 22:24'
updated_date: '2026-11-14 11:48'
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
- [ ] #1 Every model and mutation in check-tla.sh's default run, in all of specs/, passes on the laptop, each under 10 minutes (20 at most), timed with the load noted
- [ ] #2 For each replaced model, a written argument of which behaviours it covered and which new model covers each
- [ ] #3 Every mutation still finds its violation; any mutation that relied on a long model is caught by a short one
- [ ] #4 Long models kept only as deep checks, skipped by the default run, runnable explicitly, documented in the script header
- [ ] #5 doc-7's list of models, the cfg comments and OPEN_QUESTIONS.md (# Decisions) updated
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Time every model and mutation in specs/ on the laptop (load noted) to find all over 10 min.
2. Split each long model exactly where possible: a knob fixing a choice each behaviour makes at most once, one model per value, the models' union every behaviour of the original (so every reachable state, so every invariant violation). Knobs as overridable definitions in Bucket.tla, defaults no-ops (state counts of existing models unchanged): Takers (pattern of the hosts of the first takings, symmetric under HostPerms, with a history var takers), ForkTo (fork child IDs), PassCells (cells of the first passes).
3. Candidates: BucketStrays by Takers (11/121/122), BucketTakeovers by Takers (3- or 4-take patterns), BucketForks by Takers or PassCells, BucketOverlap by ForkTo {2}/{3}. Measure part sizes; refine splits where a part is over 10 min.
4. Where a split can't fit, keep the original as a deep model (cfg tag \* deep, skipped by default, run by name or 'deep') and say why.
5. Coverage: per original, the behaviours it exists for and the part covering each; the bugs each long model found (TASK-174, 183, 383, BucketOverlap's clone in its origin's cell) re-found by the parts as hand mutants; every mutation run.
6. Docs: doc-7 model list, cfg comments, check-tla.sh header, OPEN_QUESTIONS decisions. Full default run; upstream-changes.py --check; commit; push to t3/tla-decompose.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Baseline (laptop, 2026-11-13): every other model and mutation in specs/ under 5 min (slowest NoPinsVoidStrays 287 s, run beside probes); only BucketForks, BucketOverlap, BucketStrays, BucketTakeovers need splitting.
Approach changed from scenario families to exact splits: knobs in Bucket.tla (Takers, TakePhases, CommitPhases, CommitTakes, ForkTo, PassCells), parts as '\* part:' lines of the whole's cfg, run by check-tla.sh; 'check-tla.sh cover' dumps the states of the whole and of every part on a small model and fails unless they match (symmetric states compared by orbit). Negative test: one part removed, 21346 states missed, FAIL.
Dead end on the way: TakePhases as a history of steps multiplied part sizes 2-5x; now sets of allowed steps, no history. PassTakes/PassCommits (counts at pass start) left a dominant early part; replaced.
Hand mutants on single parts (sandbox copy): pinsVoidStrays and pinsLateStrays removed -> BucketStrays part {11,21,31} finds NoAckedLost; recordsKept (TASK-174) -> BucketForks part PassCells=2 {11,2x} finds NothingNeededGone; deletionsFence (TASK-183) -> PassCells=1 {11,2x} NothingNeededGone; CloneRefs across cells only (BucketOverlap's find) -> part ForkTo={3}, PassCells=13 NothingNeededGone.
<!-- SECTION:NOTES:END -->
