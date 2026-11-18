---
id: TASK-539.12
title: 'Hardening: a moment of disk-full does not fail stores'
status: To Do
assignee: []
created_date: '2026-11-18 14:17'
labels:
  - extentd
  - disk
dependencies: []
documentation:
  - backlog/docs/doc-22 - Disk-governor-the-hosts-disk-as-a-managed-resource.md
parent_task_id: TASK-539
priority: high
type: bug
ordinal: 944000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The headroom is not a proof against neighbours. Today one ENOSPC write-back fails a store at its next flush even after space returns, the acknowledgement that frees space is store-fatal, and a failing store's pages stall every tenant's writers (TASK-517). (doc-22 task T13.)
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 TASK-517 is done: a writer held at throttle while write-back drains nothing for a full flusher pass gets the write-back's error, and another store's writer is not held (two-store test).
- [ ] #2 New task, filed and done: Cached::sync reports a kept write-back failure only if writing back again fails. A test shows an ENOSPC write-back followed by success leaving the flush Ok and the store whole.
- [ ] #3 New task, filed and done: in acknowledged, a failed start-file write returns the error without fail, and the uploader retries; files leave only after it succeeds (MemDisk fail_after test).
- [ ] #4 New task, filed and done: host ENOSPC from create_open or create_raw outside `failing` maps to Errno::ENOSPC, not EIO (test).
- [ ] #5 New tasks, filed: a framing in a copy that hits ENOSPC is re-queued rather than failing the store (local_cell sim and crash suites pass); and a failed store's WAL stays on disk until it is reopened.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
