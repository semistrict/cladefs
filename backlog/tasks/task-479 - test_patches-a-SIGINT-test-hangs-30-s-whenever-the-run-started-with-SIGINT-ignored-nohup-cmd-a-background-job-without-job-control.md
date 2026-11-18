---
id: TASK-479
title: >-
  test_patches: a SIGINT test hangs 30 s whenever the run started with SIGINT
  ignored (nohup cmd &, a background job without job control)
status: Done
assignee: []
created_date: '2026-11-16 04:15'
updated_date: '2026-11-16 04:15'
labels:
  - scripts
  - flake
dependencies: []
priority: medium
ordinal: 874000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
scripts/tests/test_patches.py's test_a_sigint_does_the_same (TASK-444.2.5) sends its runner, a bash running patch_run, SIGINT. A shell started with SIGINT ignored can't trap it, and a test run started in the background without job control inherits it so: on ubu1, scripts/test.sh under `nohup setsid bash script &` timed out twice. It passed alone (0.26 s), and timed out the same under `trap '' INT`. Fix: Stoppable.start gives its runner SIGINT's default disposition.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 test_patches passes with SIGINT ignored at the start of the run
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Stoppable.start gives its runner SIGINT's default disposition (preexec_fn). Before: under `trap '' INT` on ubu1 the test timed out at 30 s. After: test_patches 12/12 on the Mac both with SIGINT ignored and without.
<!-- SECTION:FINAL_SUMMARY:END -->
