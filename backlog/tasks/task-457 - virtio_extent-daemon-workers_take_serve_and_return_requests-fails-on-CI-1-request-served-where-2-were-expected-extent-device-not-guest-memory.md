---
id: TASK-457
title: >-
  virtio_extent::daemon workers_take_serve_and_return_requests fails on CI: 1
  request served where 2 were expected, "extent device: not guest memory"
status: Done
assignee: []
created_date: '2026-11-15 13:33'
updated_date: '2026-11-15 14:26'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 861000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Main CI run 43712981845 (836047da0, a backlog/docs-only commit, so the failure is not from it) failed test (workspace) on this test alone: crates/virtio_extent/tests/daemon.rs:809 asserted 2 and got 1, with stderr "ERROR virtio_extent::requests::linux: extent device: not guest memory". The previous main run (43706034163, f60afee36) passed it, so it is intermittent: a request whose buffer the worker found outside guest memory, or a race in how the test sets up guest memory or counts what was served.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The cause is found and fixed; the test asserts the exact count and passes 200 runs in a row on ubu1 under fxjob
- [x] #2 If it was a race in the code rather than the test, a test or model that catches it
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Duplicate of TASK-458, the same failure (daemon.rs:809 is the interrupt count, not a served count; 'not guest memory' is request 4's buffers, an Err on purpose). Fixed and evidenced there.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Duplicate of TASK-458: the test read the interrupt count before the worker's second interrupt (returned, then interrupted, as virtio orders it). TASK-458 reads it once the workers stop; 0 of 5000 runs failed under load on ubu1 against 167 of 5000 before. AC #2: it was the test, not the code.
<!-- SECTION:FINAL_SUMMARY:END -->
