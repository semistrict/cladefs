---
id: TASK-458
title: >-
  virtio_extent::daemon workers_take_serve_and_return_requests fails 'interrupt
  count left 1, right 2' in CI: it reads the interrupt count once both requests
  are returned, before the worker signals the second
status: To Do
assignee: []
created_date: '2026-11-15 13:37'
labels:
  - testing
  - flake
dependencies: []
references:
  - crates/virtio_extent/tests/daemon.rs
priority: medium
type: bug
ordinal: 867000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main's CI run 43712981845 (836047da0, 2026-11-15) failed test (workspace) on this test, in its run and again on nextest's retry: crates/virtio_extent/tests/daemon.rs:809, assert_eq!(interrupt.0.load(Ordering::Relaxed), 2) gave left 1, right 2. The test waits on returns.recv() twice (requests 3 and 4, the second 'not guest memory'), then reads CountedQueue's interrupt count once. The worker returns a request to FakeQueue (its mpsc send) before it signals the queue's interrupt, so the test can see both returns while the second interrupt is still to come. Found by TASK-330.1 checking main's CI before pushing. The test came with TASK-417 (93dd6343d).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The test waits for the interrupts it expects (or the worker signals before it returns), and still fails when the worker signals once for two requests
- [ ] #2 The test passes 200 runs in a row under load (cargo nextest run -p virtio_extent --test daemon, stress-ng or parallel runs beside it), on ubu1 under fxjob
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
