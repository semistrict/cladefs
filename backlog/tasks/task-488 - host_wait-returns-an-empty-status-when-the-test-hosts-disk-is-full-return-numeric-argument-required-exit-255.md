---
id: TASK-488
title: >-
  host_wait returns an empty status when the test host's disk is full: "return:
  : numeric argument required", exit 255
status: To Do
assignee: []
created_date: '2026-11-16 08:34'
labels:
  - bug
  - scripts
dependencies: []
ordinal: 881000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-16 (run t474c on ubu1): the scratch disk filled (No space left on device) while the job ran; the job's shell could not write its status ("job: line 32: echo: write error"), so host_side wait said "status " with nothing after it, and scripts/lib/host.sh's host_wait ran `return "${said##*status }"` with an empty string: bash prints "return: : numeric argument required" and the driver exits 255, saying nothing of why. The cause (the disk) is in remote.log, not in what the driver says.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 host_wait treats a status line without a number as the job lost, saying so and why (its log fetched), and returns a nonzero status of its own
- [ ] #2 A test in scripts/tests covers a job whose status line is empty
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
