---
id: TASK-465
title: >-
  Shuttle: a scope's last scoped thread ending wakes the scope's thread whatever
  it waits on; a Condvar wait then panics
status: To Do
assignee: []
created_date: '2026-11-15 19:36'
labels:
  - testing
dependencies: []
priority: low
ordinal: 872000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
shuttle-std 0.1.4 thread.rs Scope::spawn: as the last scoped thread ends it calls unblock() on the scope's main task unconditionally. If that task is blocked on a Condvar (not in the scope's join), Condvar::wait resumes with status Waiting and panics 'should not have been woken while in Waiting status' (condvar.rs:198). Found by TASK-433's page cache under extent_store's Shuttle crash rounds (round 141): durability::at_once's caller syncs a file itself while helpers run, and the sync waited on a Condvar for the write-back in flight. Worked around in extent_store::pages by waiting on a Mutex (per-file write-back lock, Linux's I_SYNC), whose block_on re-blocks on spurious wakes. Fix upstream: unblock the main task only if it waits in the scope's join; or make Condvar::wait tolerate spurious wakeups (its own TODO). Any Condvar wait on a scope's thread while its scoped threads run is exposed.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
