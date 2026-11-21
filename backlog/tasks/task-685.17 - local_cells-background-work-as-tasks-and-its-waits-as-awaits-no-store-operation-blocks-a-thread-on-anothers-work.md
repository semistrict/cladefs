---
id: TASK-685.17
title: >-
  local_cell's background work as tasks and its waits as awaits: no store
  operation blocks a thread on another's work
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-21 01:47'
updated_date: '2026-11-21 01:48'
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

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. An async notification under the sync seam (crate::sync): event_listener::Event built normally and under madsim; under Shuttle the same, its listeners' wakers scheduling Shuttle's futures. Waits become loops: listen, lock, check, unlock, await (as upload::Shared::wait does), never a guard held across an await.
2. The store's waiting calls async, one at a time, each with its callers: wait_for_writings and wait_framed_through (under close_now, close_now_unless_filling, the commit path), wait_framed, close's wait for preparing segments, wait_until_writes_resume; Locked::wait gone. Their callers in cladefs await them (open_cell's commit and close paths are async already); the FUSE storage path awaits on its own executor (unblock).
3. The work they wait for as tasks: framing (framing::on_threads, 'store packing ahead') spawned through a Spawn of the store's, the blocking pool built normally, a task under madsim (so that a waiter can find it pending and the scheduler interleaves them); durability's scoped syncing threads as joined blocking tasks; the page flusher a task built normally too; views' published condvar an event.
4. Shuttle and loom models kept: each converted wait checked by its existing model (local_cell::shuttle, loom), run before and after; local_cell's sim and the full-stack simulation over seeds on asahimbp; fs benches on ubu1 for framing's throughput (no regression past noise).
5. Then TASK-685.18 (cladefsd's blocks! sites) and TASK-685.19 (tests async).
<!-- SECTION:PLAN:END -->
