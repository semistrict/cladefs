---
id: TASK-490
title: >-
  Mutant patches forks-take-branches-claimed and uploads-copied-each-try no
  longer apply after TASK-449: test.sh's scripts part fails on linux-batch
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-16 08:52'
updated_date: '2026-11-16 11:07'
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
- [x] #1 Both patches re-anchored on the code TASK-449 left, the same mutation each
- [ ] #2 forks-take-branches-claimed is killed by its Killer remote::tests::forks_to_a_branch_the_bucket_has_fail_with_eexist (macOS) and uploads-copied-each-try by extentd::tests::soak::a_degraded_bucket_costs_the_daemon_nothing_it_does_not_bound (ubu1, fxjob)
- [x] #3 crates/cladefs/mutants.sh --check reports 0 broken; CLADEFS_TEST_PARTS=scripts scripts/test.sh passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Re-anchor each patch by making its mutation by hand on the code TASK-449 left and diffing it, header kept. 2. Run forks-take-branches-claimed with its killer (macOS) and the killer on the clean tree. 3. Compile uploads-copied-each-try (killer is extentd's, Linux-only). 4. mutants.sh --check; CLADEFS_TEST_PARTS=scripts scripts/test.sh.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
forks-take-branches-claimed: same mutation; host_store's look is now only a writable fork's (TASK-449: a point's fork makes none, fork_point's record create refusing another's), so the hunk keeps the is_point guard and drops only the look's refusal; record_fork_under's two hunks unchanged, re-offset. Header notes the writable-only scope. macOS: mutants.sh forks-take-branches-claimed -> killed (remote.rs:3826 left (Ok(()), true) right (Err(EEXIST), false)); killer alone on the clean tree passes.
uploads-copied-each-try: same mutation (Bytes::copy_from_slice per try in bucket::create and update), context re-anchored on Created(version)/Updated(version). Compiles on macOS (cargo check -p extent_remote); killer extentd::tests::soak::... is Linux-only (extentd cfg(target_os=linux)): unrun, needs ubu1 under fxjob.
mutants.sh --check: 22 checked, 0 broken. CLADEFS_TEST_PARTS=scripts scripts/test.sh passes on macOS (501 python tests OK, every mutants check 0 broken).

2026-11-16: Linux, ubu1 under fxjob, the batch rebased on main ffd513512 (TASK-475's cladefsd) at eb13f8ca7: scripts/test.sh clippy and workspace, 2854 passed, 88 skipped. Pushed to main. mutants.sh --check 22 applying. The soak killer's run on ubu1 (317 s) failed on TASK-493's flusher panic, not on its own budget check: AC #2 open until TASK-493 lands and the mutant is rerun.
<!-- SECTION:NOTES:END -->
