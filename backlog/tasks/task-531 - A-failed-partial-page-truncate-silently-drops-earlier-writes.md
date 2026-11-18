---
id: TASK-531
title: A failed partial-page truncate silently drops earlier writes
status: To Do
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
- [ ] #1 An injected retained-page read failure leaves file size and all earlier writes intact immediately and after the next successful sync and reopen.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
