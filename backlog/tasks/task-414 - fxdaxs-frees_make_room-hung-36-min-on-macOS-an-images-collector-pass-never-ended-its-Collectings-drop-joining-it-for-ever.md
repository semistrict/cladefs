---
id: TASK-414
title: >-
  cladefs's frees_make_room hung 36 min on macOS: an image's collector pass
  never ended, its Collecting's drop joining it for ever
status: To Do
assignee: []
created_date: '2026-11-14 20:45'
updated_date: '2026-11-15 11:36'
labels:
  - testing
  - gc
dependencies: []
priority: medium
type: bug
ordinal: 805000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen 2026-11-14 in scripts/test.sh's workspace part on macOS (commit dddfea15e, TASK-400/401 on origin/main 8391d2e3d; the Mac loaded by other sessions' sim runs): cladefs storage::image::tests::frees_make_room ran 36 min at 0.31 s of CPU, killed by hand. Its stacks (sample): the test thread in ImageDevice's drop -> Kept -> Collecting::drop, joining the collector's thread; the collector thread ('cladefs collect') in storage::collect::pass -> extent_store::upload::block_on(Collector::collect(bucket, ..)), parked in the current-thread runtime's kevent (a timer pending), no task runnable; the blocking pool's thread idle (condvar wait_timeout); the uploader idle in block_in_place; no store framer or durable thread alive. Collector::collect awaits the file:// bucket alone (take the cell's lease, list, collect, release: TASK-395.6's cells and passes, landed the same day). Not reproduced: 20 runs alone and 40 more 8 at a time, all passed in seconds. Find what the pass waits for (a lost wakeup, a lease wait, a blocking task's result) and make the drop unable to hang.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The pass's wait found and fixed, or shown bounded, with a test that hangs without the fix
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
- [ ] #4 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
<!-- DOD:END -->
