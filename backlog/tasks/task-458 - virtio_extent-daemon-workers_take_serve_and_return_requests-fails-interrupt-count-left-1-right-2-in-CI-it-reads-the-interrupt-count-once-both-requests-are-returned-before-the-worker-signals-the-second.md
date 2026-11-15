---
id: TASK-458
title: >-
  virtio_extent::daemon workers_take_serve_and_return_requests fails 'interrupt
  count left 1, right 2' in CI: it reads the interrupt count once both requests
  are returned, before the worker signals the second
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-15 13:37'
updated_date: '2026-11-15 14:26'
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
- [x] #1 The test waits for the interrupts it expects (or the worker signals before it returns), and still fails when the worker signals once for two requests
- [x] #2 The test passes 200 runs in a row under load (cargo nextest run -p virtio_extent --test daemon, stress-ng or parallel runs beside it), on ubu1 under fxjob
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. Decide the promise: Shared::complete adds the used element, then raises the interrupt, as virtio orders them; a driver may see a request returned before its interrupt, so the code's order stays and the test is wrong.
2. The test reads the interrupt count once workers.stop() returns: stop joins the worker after Requests::wait_idle, whose count drops only after complete() (used element and interrupt) ran; the second start's count asserted too (3).
3. Evidence on ubu1 under fxjob, stress-ng --cpu 4 beside it: the old test looped (failure rate), a mutant sleeping 20 ms between used element and interrupt (old test fails every run, new passes), a mutant dropping the interrupt of a request with nothing written (new test fails left 1 right 2), the new test 400 runs under load.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision: the code's order is right. Shared::complete (crates/virtio_extent/src/workers.rs) adds the used element, then notifies, as virtio orders them (used ring before the interrupt); a driver polling the ring may see a request before its interrupt, which is what FakeQueue's send models. So the test reads the count after workers.stop(): stop joins the worker after Requests::wait_idle, whose serving count drops only after complete() returned (element and interrupt), under the same mutex, so the count is settled then. The second start's count (3) is asserted too.
Evidence, ubu1 under fxjob, stress-ng --cpu 4 in the same job, the test binary run alone in a loop: old test 3 of 400 and 167 of 5000 failed (3.3%, left 1 right 2); new test 0 of 400 and 0 of 5000. Mutant sleeping 20 ms between element and interrupt: old test 20 of 20 failed (left 0), new 20 of 20 passed. Mutant skipping the interrupt of a request with nothing written (len > 0): new test 5 of 5 failed (left 1, right 2, at the count after stop). cargo nextest run -p virtio_extent on ubu1: 43 of 43 passed. scripts/upstream-changes.py --check passes. TASK-457 (filed the same hour by the rename's session) is the same failure, closed as its duplicate.
<!-- SECTION:NOTES:END -->
