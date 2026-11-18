---
id: TASK-650
title: >-
  Daemon tests abort with SIGABRT at process exit after passing (a different
  test each run)
status: To Do
assignee: []
created_date: '2026-11-18 21:17'
labels:
  - cladefsd
  - flaky
dependencies: []
priority: medium
type: bug
ordinal: 1032000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On ubu1 (2026-11-18), three full workspace runs each had one test process abort with SIGABRT after its test reported ok: cladefs::tenants tenants_hold_their_own_pages (twice, on main 6b7b54e24 too), cladefsd::tests::snapshots_after_private_pages_are_rebound_restore_their_bytes once. stderr shows nothing but the DAX window INFO lines, then "(test aborted with signal 6: SIGABRT)", so the abort is in teardown: a destructor, a thread still running as the process exits (the daemon's serve thread, uffd fault threads), or a runtime dropped from within itself. Unrelated to TASK-633.9 (seen on main). LIBC_FATAL_STDERR_=1 is set by test.sh; no glibc message was printed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The cause of the exit-time abort is found (core dump or a backtrace on abort under the test harness)
- [ ] #2 The affected tests exit cleanly in 50 repeated runs under full-suite load on ubu1
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
