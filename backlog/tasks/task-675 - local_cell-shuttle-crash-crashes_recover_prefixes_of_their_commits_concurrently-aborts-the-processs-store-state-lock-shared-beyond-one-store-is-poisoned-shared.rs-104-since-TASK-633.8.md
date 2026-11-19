---
id: TASK-675
title: >-
  local_cell shuttle
  crash::crashes_recover_prefixes_of_their_commits_concurrently aborts: 'the
  process's store state lock, shared beyond one store, is poisoned'
  (shared.rs:104), since TASK-633.8
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-19 22:27'
updated_date: '2026-11-19 23:36'
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

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. framed::unpacked() (d5133b1cc, TASK-603): a std LazyLock static holding a Shuttle Mutex, shared by every Shuttle execution in the process: an execution taking a lock another made corrupts Shuttle's state (RefCell already borrowed in batch_semaphore), then the store state lock poisons. Under Shuttle, per execution (shuttle::lazy_static), and its lock std's (a leaf lock, as frames.rs's): a framed file's drop, which takes it, is no point of a schedule.
2. That exposed a determinism hole: round 216 of crashes_recover_prefixes_of_their_commits_concurrently left different first crashes in its two runs. Traced (MemDisk op and dir-change logs, both runs identical; then write order): Uncommitted's open extents (HashMap) and Files' framed files (HashMap) drop in the map's order, which differs run to run; each last holder of a raw file writes its dirty pages back as it drops, so which write a crash's budget cuts off differed. Drop them in a decided order: open extents by opening (as open_extents orders them, TASK-79.79), framed files least lately read first.
3. Verify: local_cell's shuttle tests, CI's shuttle part, clippy.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Diagnosis: at eabaadd0a the crash test passes, serially too; at d5133b1cc with fix 1 alone it diverges at round 216 (full run, deterministic; alone it passed once: depends on the hash seeds). Experiments: cache disabled, memory of forgotten parts leaked: still diverges; Framed's drop not forgetting: passes (schedule shift only). Op traces of round 216's two runs identical up to two write-backs (inodes 33, 34) swapped; backtrace: HashMap<ExtentId, Open> drop -> RawFile -> CachedFile -> pages::Shared::drop -> flush -> write_runs. With both fixes: local_cell shuttle 37/37 on macOS.
<!-- SECTION:NOTES:END -->
