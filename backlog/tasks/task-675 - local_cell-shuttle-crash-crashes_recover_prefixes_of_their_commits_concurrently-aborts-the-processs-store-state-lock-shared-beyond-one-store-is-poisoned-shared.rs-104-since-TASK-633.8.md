---
id: TASK-675
title: >-
  local_cell shuttle
  crash::crashes_recover_prefixes_of_their_commits_concurrently aborts: 'the
  process's store state lock, shared beyond one store, is poisoned'
  (shared.rs:104), since TASK-633.8
status: To Do
assignee: []
created_date: '2026-11-19 22:27'
labels:
  - ci
  - bug
dependencies: []
priority: high
ordinal: 1057000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
test (shuttle) and macos (shuttle) fail on main since 12ed2aad6 (run 44579460439; e0bfaeaed's run passed them), still on 28a38a315 (run 44615661631, job 135883517860). SIGABRT in local_cell::shuttle crash::crashes_recover_prefixes_of_their_commits_concurrently: a task panics first inside shuttle-engine 0.1.5 (future/batch_semaphore.rs:1328:54 and :579:36 on Linux; runtime/execution.rs:217:50, shuttle-std condvar.rs:198:43, mutex.rs:70:48 on macOS), then the main thread panics at crates/local_cell/src/shared.rs:104:13 'the process's store state lock, shared beyond one store, is poisoned'. The CI log prints the failing schedule for shuttle::replay. Commits in range touching crates/local_cell: TASK-633.8's 63bf868fb, 305fe4cb7, 7bd45a297 (a store's opening begins its version's mirror on its uploader's runtime), 63873be67. Diagnose the first panic (the poisoned lock is its consequence), from the replayed schedule.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The failure replays locally from CI's schedule and its first panic is explained
- [ ] #2 Fixed; crashes_recover_prefixes_of_their_commits_concurrently passes, and CI's test (shuttle) and macos (shuttle) pass on main
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
