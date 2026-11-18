---
id: TASK-511
title: >-
  cladefs::control open_stores_are_listed_with_their_takings_and_uploads aborts
  at times after passing
status: To Do
assignee: []
created_date: '2026-11-18 01:37'
labels:
  - cladefs
  - flaky
dependencies: []
ordinal: 903000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-18 on ubu1 in a whole-workspace nextest under load (branch t3/cell-is-one-filesystem; crates/cladefs/tests/control.rs unchanged from origin/main): the test reported ok, then the process aborted with SIGABRT and no panic message in its captured output. 6 reruns alone passed. Likely a thread outliving the test aborting (the daemon's abort-on-panic hook, or a shared lock's poison) as the process exits; needs a backtrace from a failing run (RUST_BACKTRACE=1, under load).
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
