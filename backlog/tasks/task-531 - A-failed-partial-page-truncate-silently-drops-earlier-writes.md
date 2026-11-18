---
id: TASK-531
title: A failed partial-page truncate silently drops earlier writes
status: In Progress
assignee: []
created_date: '2026-11-18 12:25'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 921000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 16.

In `crates/extent_files/src/pages.rs:659`, shrinking removes all dirty chunks beyond the new end before the fallible read of the retained partial page. On error, the old file size stays in force, but reads can return old committed bytes in place of previously accepted writes. A later successful sync preserves the wrong contents. Prepare the retained page before discarding any dirty chunks. The existing crash test in `crates/extent_files/tests/crash.rs` checks committed images but does not combine an outstanding overwrite with a read failure during shrink.

Isolated arm64 macOS validation. Commit three pages filled with byte 1. Overwrite page three with byte 7, verify the read, then truncate to one byte while failing the read of page one. Truncate returned an error and size stayed 12,288. Page three immediately read as 1. After sync and reopen it still read as 1. Without injection, truncate succeeded and page three was past EOF.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 An injected retained-page read failure leaves file size and all earlier writes intact immediately and after the next successful sync and reopen.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
The storage audit found that the crash oracle missed an accepted dirty overwrite when the retained page read failed. The file_operations regression compares the whole live image and the recovered image after sync. Preparing the partial page before splitting the dirty map makes the destructive part of shrink infallible. Inspection of dirty_chunk confirmed that it publishes a newly read page only after the read succeeds.

The unchanged regression failed in the macOS audit run and passed against the implementation. Both implementation reviews reported no findings.
The regression also passed on ubu1 in run r516e against 24c6081a6 with the production patch. All 19 extent_files tests passed on both platforms. The macOS crate run passed 412 of 413 (TASK-530 failed), and its workspace run stopped on TASK-521. The ubu1 crate run passed 681 of 684 (TASK-519, TASK-520 and TASK-522 failed, as in the baseline audit). Workspace qualification remains open.
<!-- SECTION:NOTES:END -->
