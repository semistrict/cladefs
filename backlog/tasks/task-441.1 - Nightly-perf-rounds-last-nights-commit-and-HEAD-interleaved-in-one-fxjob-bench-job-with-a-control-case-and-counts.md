---
id: TASK-441.1
title: >-
  Nightly perf rounds: last night's commit and HEAD interleaved in one fxjob
  --bench job, with a control case and counts
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 11:46'
updated_date: '2026-11-15 23:42'
labels:
  - testing
  - ubu1
  - perf
dependencies: []
parent_task_id: TASK-441
priority: high
ordinal: 831000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The measuring half of the nightly. scripts/bench-ab-host.sh already builds two sides as work and interleaves rounds in the bench slot; a single run per commit is no A/B on ubu1's disk (TASK-395.17: 320 s against 559 s were single runs). The baseline side is the commit the last night measured, its artifacts kept from that night so one side builds. The control case tells a host that moved from a commit that did.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Each round measures both sides and the control; a metric is reported moved only when it moved in the same direction past its bound in every kept round and the control did not
- [ ] #2 Counts (written bytes per guest byte by kind, daemon peak memory, device requests, bucket requests) are compared exactly beside the timings
- [x] #3 The baseline's artifacts are last night's: a night builds one tree
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. scripts/lib/host.sh: CLADEFS_HOST=local runs a run's host half on this machine, no ssh (the nightly runs on ubu1 itself, from cron).
2. scripts/lib/bench-ab-linux-host.sh: CLADEFS_AB_KEEP=DIR keeps each side's artifacts by its source revision and kernel build, so a side built before builds nothing (the baseline is last night's HEAD); a side that fails to build says which in RESULTS/build-failed; CLADEFS_AB_COUNTS=1 runs scripts/counts.sh (the three exact count benchmarks: bucket_lifecycle_by_phase, bench_fork_extent_memory, gc-sweep's dead cells) in each side's build, kept with its artifacts; CLADEFS_AB_RERUN=1 reruns once each round scripts/ab-rounds.py discards (contaminated, or its control moved).
3. scripts/ab-rounds.py: the round-by-round comparison: per (case, workload), each round's B/A ratio of medians; a round left out when contaminated or when the control case's median ratio moved past CLADEFS_AB_CONTROL_BOUND (5%); a lost slot (round and rerun both left out) is NO RESULT; moved only when every slot moved the same way past its bound (CLADEFS_AB_BOUND 10%, CLADEFS_AB_BOUNDS per metric); counts (results in count units, count observations, counts.sh's outputs) compared exactly (CLADEFS_AB_COUNT_BOUND 0), contaminated rounds kept.
4. Tests (scripts/tests/test_ab_rounds.py, test_bench_ab_host.py, test_host.py) on fixtures shaped as ubu1's bench JSON; hand mutants of each rule.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Built on macOS (no ubu1 access in this task). scripts/lib/host.sh: CLADEFS_HOST=local (host_shell: the run's host half in the home, no ssh). scripts/lib/bench-ab-linux-host.sh: CLADEFS_AB_KEEP (ab_key = source revision + kernel build; kept dirs touched on use, CLADEFS_AB_KEEP_COUNT newest kept; a dirty or unknown revision is not kept), RESULTS/build-failed naming the side, CLADEFS_AB_COUNTS (scripts/counts.sh into the kept artifacts' counts, copied to RESULTS/NAME-counts), CLADEFS_AB_RERUN (ab-rounds.py discarded; reruns.txt). Side maps became plain variables (tree_a, artifacts_b, `of`) so the build can be sourced and tested under macOS's bash 3.2. scripts/ab-rounds.py: per-round ratios of medians, control = median of the control's timing ratios (5%), slots with reruns, NO RESULT on a lost slot, moved past 10% the same way in every slot; counts (contamination.COUNTS units, observations *-requests/-bytes/-reads and extentd-* but times, counts.sh's lifecycle/fork-memory/gc tables) exact in every round, contaminated rounds kept; broken = failures/cases/count benchmarks only B has. Tests: test_ab_rounds.py (20), test_bench_ab_host.py Kept (6), test_host.py local host; 15 hand mutants all killed (moved-in-some, control ignored, contamination ignored, rerun ignored, lost slot judged, non-strict bound, counts skipping contaminated rounds, counts changed-in-some, inexact counts, no note on repeat, Done task noted, four bad nights, no skip, one push, kept artifacts ignored).

Validation (macOS): python3 -m unittest discover -s scripts/tests 483 tests OK; CLADEFS_TEST_PARTS=scripts scripts/test.sh passed all but its last step, firecracker-cargo.sh --check, a stale vendor/firecracker.lock this branch doesn't touch (filed TASK-469); scripts/upstream-changes.py --check passes; shellcheck clean on every shell file touched. AC #1, #3 checked on the tests (the comparison's rules on fixtures shaped as ubu1's results; the host script's own build() sourced with stubs, two nights, one tree built the second). Open: AC #2 until scripts/counts.sh has run on Linux (its parsers are tested on the macOS runs' outputs, it never ran here); DoD #1 (no Rust changed; the Linux run needs ubu1); the first real night on ubu1 (TASK-441's ACs).

Counts also take each daemon's scripts/budgets.py figures from a round's logs/budgets.json (written bytes per guest byte by kind, exact; max_anon within a default 10%, a sampled peak: ab-rounds.py NAMED), test_ab_rounds.test_each_daemons_budget_figures_are_counts.
<!-- SECTION:NOTES:END -->
