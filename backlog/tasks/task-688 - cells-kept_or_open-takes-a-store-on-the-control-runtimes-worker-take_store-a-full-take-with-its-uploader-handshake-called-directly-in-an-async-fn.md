---
id: TASK-688
title: >-
  cells::kept_or_open takes a store on the control runtime's worker: take_store,
  a full take with its uploader handshake, called directly in an async fn
status: To Do
assignee: []
created_date: '2026-11-20 07:39'
labels:
  - bug
  - cladefsd
dependencies: []
ordinal: 1078000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-20 by the async-rewrite inventory (doc-24). crates/cladefs/src/cladefsd/cells.rs kept_or_open: `Ok((keep(&hold, take_store(from, None, capacity)?), Opened::Now))` runs take_store (bucket requests, the uploader's start handshake `start.recv()`) inline in an async fn, not in `blocking()` as cells.rs's other store work is. On the control path (fork_of_version, at_version, fork_version via ForkCell/restores) it holds a worker of the control runtime for the whole take, seconds from a slow bucket (TASK-443.6 measured ~11 s), so other control calls queue behind it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 take_store in kept_or_open runs where the rest of cells.rs's store work does (blocking(), or its async form once TASK-685.3's steps make it one), with a test or argument that the control runtime keeps serving meanwhile
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
