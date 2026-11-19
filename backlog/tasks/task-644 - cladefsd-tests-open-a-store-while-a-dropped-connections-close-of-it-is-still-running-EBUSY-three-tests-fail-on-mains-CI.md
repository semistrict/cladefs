---
id: TASK-644
title: >-
  cladefsd tests open a store while a dropped connection's close of it is still
  running: EBUSY (three tests fail on main's CI)
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 20:37'
updated_date: '2026-11-19 22:30'
labels:
  - testing
dependencies: []
priority: high
type: bug
ordinal: 1028000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
CI runs 44353952388 (8111fb784) and 44380042393 (ca72b5814) on main, 2026-11-18: test (workspace) fails in both, in these tests:
- cladefsd::tests::faults_waiting_for_object_storage_hold_up_no_request (tests.rs:4905, "opening the extent store at .../elsewhere/store: EBUSY")
- cladefsd::tests::hostile::page_ins_in_flight_are_bounded (tests/hostile.rs:2259, the same EBUSY)
- cladefsd::tests::restores_over_what_a_failed_cleanup_left_fork_anew (tests.rs:4478: left (Err(EBUSY), false), right (Err(EIO), false))
The first run also failed a_zygotes_restores_open_its_copy_once and restores_fork_anew_over_forks_left_unmade.

The Multi-Region session's diagnosis, not yet checked against the code: a test drops its client connection and the daemon closes that connection's store in the background. The next open or fork of the cell syncs or takes the cell's stores first, and if the close hasn't finished it answers EBUSY. On ubu1, five runs of the daemon module came back clean, and the Fix-mutants session saw page_ins_in_flight_are_bounded fail once there with EBUSY. So it shows up under CI's load and rarely on ubu1.

Related, not the same: TASK-443.6 (restores of a point still being opened fail EBUSY after CLOSING_WAIT).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Whether EBUSY here is the daemon's fault (an open, take or fork arriving before a dropped connection's close finishes should wait for that close, not refuse) or the tests' (each should wait for the close it caused) is settled from the code and recorded in this task.
- [ ] #2 The five tests named pass 50 runs in a row on ubu1 under a parallel load like CI's (nextest --test-threads 4 alongside a build), and in main's CI test (workspace) job.
- [ ] #3 A test reproduces the race deterministically (the close held at a failpoint while the next open arrives) and goes red with the fix reverted.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
