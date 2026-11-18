---
id: TASK-539.9
title: 'Disk governor loop, modes and environment'
status: To Do
assignee: []
created_date: '2026-11-18 14:17'
labels:
  - extentd
  - disk
dependencies:
  - TASK-539.5
  - TASK-539.6
  - TASK-539.8
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: high
type: feature
ordinal: 941000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Requirements (1) to (3): a dedicated host uses all of its disk but a headroom, a fixed cap is possible, and caches give space back before the stores or a neighbour hit the wall. (doc-22 task T8.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 governor.rs has pure, unit-tested functions: disk_headroom (bytes, 'N%', default max(size/20, 4 GiB) clamped to size/2, malformed values an error), disk_allowance, spare, and the pressure hysteresis (enter spare < H, leave spare ≥ H + H/4).
- [ ] #2 disk::start(DiskConfig) runs the 'disk governor' thread every governor::INTERVAL and when woken, and is started by bin/cladefsd.rs and by tests/common/daemon.rs (64 MiB headroom). disk::govern_once implements doc-22 §5 steps 1–8.
- [ ] #3 CLADEFSD_DISK_HEADROOM and CLADEFSD_DISK_BYTES are parsed at start, and a bad value stops the daemon (tests like budgets_the_cache_cant_take_are_refused). With CLADEFSD_DISK_BYTES = N, stores plus cache stay ≤ N (test).
- [ ] #4 On a MemDisk HostDisk, growth that takes spare under the cache's bytes wakes the governor, and the cache shrinks to the limit minus a band in one pass.
- [ ] #5 Entering pressure sets wanted on every uploader registered through dir.disk(): a test shows the open segment closed and uploaded without waiting filling_wait.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
