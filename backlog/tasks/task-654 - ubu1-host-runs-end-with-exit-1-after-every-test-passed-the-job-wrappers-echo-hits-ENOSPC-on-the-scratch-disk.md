---
id: TASK-654
title: >-
  ubu1 host runs end with exit 1 after every test passed: the job wrapper's echo
  hits ENOSPC on the scratch disk
status: To Do
assignee: []
created_date: '2026-11-19 08:20'
updated_date: '2026-11-19 08:20'
labels:
  - test-host
  - ubu1
dependencies: []
priority: low
type: bug
ordinal: 1035000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-18 night on two of three scripts/test-host.sh runs (82bb2aa37, e82da0965): every test part passed ("cladefs tests: exit 0", all nextest summaries green), then the driver printed "job: line 32: echo: write error: No space left on device" and exited 1. "job" is $0 of the setsid bash -c wrapper in scripts/lib/host-remote.sh launch(); its stdout/stderr and $job/status are on /mnt/fxdax-scratch (zvol, 251G), which other agents' runs fill at times (80G free when checked after). A status write failing makes the driver report a failed run although the tests passed.
Ideas: write $job/status (a few bytes) somewhere that cannot fill (e.g. under /run or $HOME on rpool), or reserve room; have the driver tell "status unwritable" from a failing job; host_sweep older runs (run-t474e-20261116 holds 82G).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A run whose tests pass ends 0 even if the scratch disk fills at its end, or says the disk filled rather than reporting a test failure
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Seen again 2026-11-19 on runs of 11e4e53ea and b65b648c7 (every part passed, exit 1). Related: TASK-652 (fxjob starts the next job before the last one's temp dir is removed: ENOSPC at the next job's first mkdir), TASK-491 (finished runs fill the scratch disk), TASK-488 (empty status on a full disk, Done).
<!-- SECTION:NOTES:END -->
