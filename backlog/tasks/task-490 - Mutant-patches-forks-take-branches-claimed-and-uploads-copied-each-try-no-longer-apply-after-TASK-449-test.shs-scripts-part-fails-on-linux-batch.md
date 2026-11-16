---
id: TASK-490
title: >-
  Mutant patches forks-take-branches-claimed and uploads-copied-each-try no
  longer apply after TASK-449: test.sh's scripts part fails on linux-batch
status: To Do
assignee:
  - '@claude'
created_date: '2026-11-16 08:52'
labels:
  - bug
  - testing
dependencies: []
priority: high
ordinal: 884000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
crates/cladefs/mutants.sh --check on linux-batch (1d4c9428d): 'mutants: 22 checked, 2 broken': forks-take-branches-claimed.patch and uploads-copied-each-try.patch don't apply. Both apply on HEAD~5 (3fca2a5de, before TASK-449) and fail from 568767cf7 (TASK-449, Restores in 6 round trips) on: it changed crates/cladefs/src/host_store.rs fork_in (the child_free check now 'if !remote.is_point() && !remote.child_free(...)') and crates/extent_remote/src/bucket.rs create (Created names the version written). scripts/test.sh's scripts part, CI's clippy scripts job, fails on them, so TASK-449 can't land on main as is. Found by the agent fixing TASK-449's Linux-only compile error (fuse/control.rs) on linux-batch.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Both patches re-anchored on the code TASK-449 left, the same mutation each
- [ ] #2 forks-take-branches-claimed is killed by its Killer remote::tests::forks_to_a_branch_the_bucket_has_fail_with_eexist (macOS) and uploads-copied-each-try by extentd::tests::soak::a_degraded_bucket_costs_the_daemon_nothing_it_does_not_bound (ubu1, fxjob)
- [ ] #3 crates/cladefs/mutants.sh --check reports 0 broken; CLADEFS_TEST_PARTS=scripts scripts/test.sh passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
