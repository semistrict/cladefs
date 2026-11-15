---
id: TASK-446
title: >-
  bench-rounds.sh's build ignores the caller's CARGO_BUILD_JOBS and
  CARGO_TARGET_DIR: sudo drops them, so it compiles with every core of ubu1, in
  the worktree's target
status: To Do
assignee: []
created_date: '2026-11-15 12:37'
updated_date: '2026-11-15 14:52'
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
- [ ] #1 bench-rounds.sh's build runs at most 4 cargo jobs at once on ubu1 by default, or as many as the caller says, as host.sh's runs do
- [ ] #2 A caller's CARGO_TARGET_DIR is where the build goes, or the script says it takes none
- [ ] #3 A test in scripts/tests fails without the change
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
