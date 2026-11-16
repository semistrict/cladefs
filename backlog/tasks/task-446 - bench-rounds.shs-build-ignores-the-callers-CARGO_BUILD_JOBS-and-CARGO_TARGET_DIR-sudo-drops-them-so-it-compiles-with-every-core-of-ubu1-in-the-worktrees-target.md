---
id: TASK-446
title: >-
  bench-rounds.sh's build ignores the caller's CARGO_BUILD_JOBS and
  CARGO_TARGET_DIR: sudo drops them, so it compiles with every core of ubu1, in
  the worktree's target
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 12:37'
updated_date: '2026-11-16 00:43'
labels:
  - bench
  - ubu1
dependencies: []
references:
  - scripts/bench-rounds.sh
  - scripts/lib/host.sh
priority: low
ordinal: 850000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
scripts/bench-rounds.sh builds the cases' artifacts as root, `fxjob --name rounds-build --mem 8G -- nice -n 19 sudo env PATH HOME CLADEFS_*... scripts/test-e2e.sh`: sudo resets the environment, and only PATH, HOME and the CLADEFS_* variables are passed on. A caller's CARGO_BUILD_JOBS=4, ubu1's rule for builds (4 jobs at once, as scripts/lib/host.sh's host_command sets CARGO_BUILD_JOBS and NEXTEST_TEST_THREADS from CLADEFS_HOST_JOBS), never reaches cargo, which then runs one job a CPU, 16 on ubu1, at niceness 19 beside others' work; and CARGO_TARGET_DIR is dropped the same way, so the build lands in the worktree's own target whatever the caller named. Found running TASK-397's check on ubu1 (2026-11-15), which built the artifacts beforehand with CARGO_BUILD_JOBS=4 to stay within the rule. TASK-368 (every timed host driver builds as work and measures alone) is near it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 bench-rounds.sh's build runs at most 4 cargo jobs at once on ubu1 by default, or as many as the caller says, as host.sh's runs do
- [x] #2 A caller's CARGO_TARGET_DIR is where the build goes, or the script says it takes none
- [x] #3 A test in scripts/tests fails without the change
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-15 (claude): fixed in TASK-368's commit (d82edf385, branch t368): bench-rounds.sh's build passes CARGO_BUILD_JOBS (default 4) and CARGO_TARGET_DIR through sudo, and chowns that target back. scripts/tests/test_e2e_job.py's Rounds runs bench-rounds.sh under fakes (a sudo that resets the environment as sudo does): test_the_cases_build_as_work_and_each_run_boots_them_alone (jobs=4 by default) and test_the_build_takes_the_callers_bounds_on_cargo (jobs=2, the target given), both red with the change taken out. On ubu1 the build ran with CARGO_TARGET_DIR in the home, the target there, and 1,425 s of CPU over 530 s wall (about 2.7 CPUs, within 4).

2026-11-15: Linux, ubu1 under fxjob (CARGO_BUILD_JOBS=4, --test-threads 4), main 21f042594: scripts/test.sh parts workspace and go, 2817 tests passed, 83 skipped; go/extentd/v1 ok (clippy passed in the run before, on 618403778). macOS: the crates this change touched and the scripts suite (see notes above). DoD #1 checked on that evidence.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
bench-rounds.sh's build passes CARGO_BUILD_JOBS (default 4) and CARGO_TARGET_DIR through sudo (with TASK-368). Verified by test_e2e_job.py under fakes and the ubu1 bench-rounds run.
<!-- SECTION:FINAL_SUMMARY:END -->
