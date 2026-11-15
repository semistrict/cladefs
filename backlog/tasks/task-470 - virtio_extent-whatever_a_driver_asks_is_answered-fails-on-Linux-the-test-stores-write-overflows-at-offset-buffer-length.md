---
id: TASK-470
title: >-
  virtio_extent whatever_a_driver_asks_is_answered fails on Linux: the test
  store's write overflows at offset + buffer length
status: To Do
assignee: []
created_date: '2026-11-15 23:44'
labels:
  - bug
  - testing
dependencies: []
priority: medium
ordinal: 876000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-15 by the Linux workspace run on ubu1 (branch perf-linux at 618403778, which does not touch crates/virtio_extent): the proptest requests::tests::whatever_a_driver_asks_is_answered panicked "attempt to add with overflow" in the test store's write (crates/virtio_extent/src/requests.rs, `data.resize(data.len().max(at + buffer.len()), 0)` with `let mut at = offset as usize;`), then every later case failed on the poisoned mutex (requests.rs `self.state()`); shrinking gave up after 8000 iterations; minimal input not found. A driver's WRITE with an offset near u64::MAX reaches the store: either the device should refuse it (EINVAL) before the store, as a real store would need, or the fake store should answer it as the real one does. Which is the question: check what crates/cladefs's store does with such an offset. The run's log was /mnt/fxdax-scratch/perf-linux/test.log on ubu1 (removed after).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A driver write whose offset plus length overflows is answered the way the real store answers it, and the test asserts that exact answer
- [ ] #2 whatever_a_driver_asks_is_answered passes on Linux with that case in its regressions
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
