---
id: TASK-535
title: >-
  A virtio queue can report successful startup after its event loop fails to
  initialize
status: To Do
assignee: []
created_date: '2026-11-18 12:26'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 925000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 20.

At `crates/virtio_extent/src/workers.rs:171`, worker creation reports success once the thread is spawned. Creation of its epoll descriptor and registration of its queue and stop descriptors happen inside the thread at lines 206–221. Failure there logs and returns without servicing the queue or propagating startup failure. Verify startup fails on a refused epoll registration and rolls back other workers. Source-traced only.

Evidence is from source inspection. Runtime reproduction is outstanding.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An epoll creation or registration failure propagates to queue startup and any previously started workers are rolled back. A successful startup still services its queues.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
