---
id: TASK-539.12
title: 'Hardening: a moment of disk-full does not fail stores'
status: Done
assignee: []
created_date: '2026-11-18 14:17'
updated_date: '2026-11-18 19:44'
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
- [x] #1 TASK-517 is done: a writer held at throttle while write-back drains nothing for a full flusher pass gets the write-back's error, and another store's writer is not held (two-store test).
- [x] #2 New task, filed and done: Cached::sync reports a kept write-back failure only if writing back again fails. A test shows an ENOSPC write-back followed by success leaving the flush Ok and the store whole.
- [x] #3 New task, filed and done: in acknowledged, a failed start-file write returns the error without fail, and the uploader retries; files leave only after it succeeds (MemDisk fail_after test).
- [x] #4 New task, filed and done: host ENOSPC from create_open or create_raw outside `failing` maps to Errno::ENOSPC, not EIO (test).
- [x] #5 New tasks, filed: a framing in a copy that hits ENOSPC is re-queued rather than failing the store (local_cell sim and crash suites pass); and a failed store's WAL stays on disk until it is reopened.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Deviation: ACs #2-#4 asked for separate tasks; done here instead, each with its test.

Merged with main (6fb73ca0a fixed TASK-517 at the same time): main's PageCache::throttle, which gives a held writer the flusher's last pass's failure, replaced this task's failure counter; this task's sync change, acknowledgement retries and ENOSPC mapping stand. Main's write_back_reporting and File::disk_identity, used only by the walk TASK-539.5 deletes, went with it.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
PageCache::throttle gives a writer held at the hard bound the write-back's failure once one fails and nothing drained while it waited (TASK-517's audit test passes); Cached::sync clears a kept failure once writing back again succeeds (a_write_back_that_failed_then_succeeds_fails_no_sync); a failed record of an acknowledgement (wal::StartFile, two slots, the other left whole) no longer fails the store, and the uploader keeps such acknowledgements pending, retrying each pass (upload::tests::acknowledgements_a_full_disk_refuses_wait_for_room, with a wal::start failpoint); open_cell::errno maps the host's ENOSPC to ENOSPC, a failed store staying EIO (errno_tests). Filed TASK-539.16 (framing in a copy re-queued on ENOSPC) and TASK-539.17 (failed stores' backlog). local_cell 317/317 on macOS.
<!-- SECTION:FINAL_SUMMARY:END -->
