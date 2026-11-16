---
id: TASK-489
title: >-
  The nightly's first night could not run: the scratch disk was full as its
  measurement started, and a lost night is never retried
status: To Do
assignee: []
created_date: '2026-11-16 08:37'
labels:
  - bug
  - testing
  - ubu1
dependencies: []
priority: high
ordinal: 881000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
First night on ubu1, 2026-11-16 (docs/measurements/ubu1-nightly-2026-11-16T0700Z-7f4923f3): both sides built as work (02:00-03:33 EST), then the measurement job started in the bench slot and failed at once: remote.log says `mkdir: No space left on device` and `job: line 32: echo: write error: No space left on device` (the job could not write its status), then scripts/lib/host.sh host_wait failed with `return: : numeric argument required` (it returns "${said##*status }", empty when the status was never written). The night was reported "could not run: no round ran (the A/B ended 2)" and pushed. /mnt/fxdax-scratch (251 GB, the zvol all fxjob jobs share) was full at 08:33 UTC; an hour later it had 75 GB free, other agents' runs (t435 60 GB, t411f 13 GB) and the nightly's own fxdax-host run directories (83 GB) holding most of it. Three gaps: (1) nothing checks room before a night builds or measures, and the night does not say why it could not run (disk full); (2) host_wait turns a missing status into a bash error instead of a clear "status lost"; (3) nightly.py plans from the newest night's head whether or not that night measured anything, so a night that could not run is not retried the next night unless main's code moves.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A night checks the scratch disk's free space before building and before measuring, against what it needs (its run directories' sizes from earlier nights), and a night short of room says so in its report and log, naming the disk and what it found
- [ ] #2 host_wait reports a job whose status is empty or unreadable as lost, with a test that fails before
- [ ] #3 A night that could not run, or whose rounds were all discarded, does not count as measured: the next night measures the same head again, with a test
- [ ] #4 The nightly's own run directories from earlier nights are removed but for the artifacts the next night reuses, with a test
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
