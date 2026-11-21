---
id: TASK-721
title: >-
  test_patches:
  test_it_runs_its_command_with_the_mutant_in_and_returns_its_status fails now
  and then on macOS, bash printing "child setpgid (PID to PID): Operation not
  permitted" to stderr
status: To Do
assignee: []
created_date: '2026-11-21 07:01'
labels:
  - flaky
  - scripts
dependencies: []
priority: low
ordinal: 1125000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
In a full macOS run of scripts/tests (python3 -m unittest discover -s scripts/tests, 553 tests, 2026-11-21) it failed once: stdout was right ("status 7"), stderr held "scripts/lib/patches.sh: child setpgid (17193 to 17193): Operation not permitted" where the test expects nothing. Alone it passes 4 of 4. bash prints this when job control has it set a child's process group after the child already exec'd (the race between parent and child setpgid; EPERM once the child ran exec). Under load the child wins the race. scripts/test.sh's scripts part runs this suite, so CI or a laptop run can go red for nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 patches.sh no longer has bash print the setpgid warning, under load as well
- [ ] #2 The test passes in 20 full runs of the scripts suite on macOS, or a test reproduces the race and passes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
