---
id: TASK-685.17
title: >-
  local_cell's background work as tasks and its waits as awaits: no store
  operation blocks a thread on another's work
status: To Do
assignee: []
created_date: '2026-11-21 01:47'
labels:
  - async
  - simulation
dependencies: []
parent_task_id: TASK-685
priority: high
ordinal: 1121000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24's one async path, inside the store (user, 2026-11-20). Today local_cell frames segments on threads of its own (framing::on_threads, 'store packing ahead'), syncs a flush's files on scoped threads (durability), flushes pages on a thread (pages), and its operations wait for that work on the store's idle condvar (Locked::wait: close, prepared, wait_framed, wait_framed_through, close_now_unless, wait_for_writings, wait_until_writes_resume), as do views (published) and pages (flush, drained). Called from the daemon's async code (a guest's COMMIT calls commit_ordered on the IO runtime), such a wait blocks a runtime worker; under madsim, where that work runs inline and the store's calls never yield, the waits are never entered, so the simulation explores none of production's interleavings inside the store. Goal: the work tasks (CPU and disk work in the blocking pool built normally, inline under madsim), the waits async (an event per condition), the store's waiting calls async, callers awaiting them; Shuttle's and loom's models kept (their primitives under the sync seam). Disk syscalls stay synchronous.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
