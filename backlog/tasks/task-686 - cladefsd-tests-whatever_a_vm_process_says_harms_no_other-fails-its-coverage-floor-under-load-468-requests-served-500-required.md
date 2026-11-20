---
id: TASK-686
title: >-
  cladefsd::tests::whatever_a_vm_process_says_harms_no_other fails its coverage
  floor under load: 468 requests served, 500 required
status: To Do
assignee: []
created_date: '2026-11-20 05:39'
labels:
  - testing
  - flaky
dependencies: []
ordinal: 1076000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen on ubu1 2026-11-20 at a2c262210 (t3/daemon-core-2) in the full workspace run under fxjob (--test-threads 4), beside TASK-677's hostile test: tests.rs's assert!(served > 500) fails with 468 requests served. Alone it passes (658 served, 9 restores). The floor counts what the daemon served before the fuzzed VM process hung up, which falls with the host's load (the comment says 929 at least of ~1300 when it was set; TASK-153.2 saw it under the earlier floor of 1000, about 1 run in 3). A coverage floor that load moves is flaky by design: it should count something load does not change (e.g. run until N requests were served, or bound by requests rather than time).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The test's coverage check does not depend on the host's speed: it passes in a loaded workspace run on ubu1 (several runs) and still fails if every request is refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
