---
id: TASK-524
title: Sparse page write-back does quadratic work under the file lock
status: To Do
assignee: []
created_date: '2026-11-18 12:24'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 914000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 9.

In `crates/local_cell/src/pages.rs:684`, each run restarts its search at the beginning of the requested range. Line 731 scans again for remaining dirty pages. Flushing N isolated dirty pages revisits a growing clean prefix, yielding quadratic page inspections. Index dirty pages or advance through a bounded generation. Verify inspection counts for increasing sparse files and preserve concurrent redirty semantics.

Isolated arm64 macOS validation. Instrument the two page traversals in a temporary copy of the page cache. Write 1,024 full pages to a memory sink, then sync, with either consecutive offsets or one hole between pages. Consecutive pages required 3,587 run-search visits and 2,563 dirty-check visits over four writes. Sparse pages required 526,847 and 525,823 over 1,024 writes. At 512 sparse pages the two counts were 132,351 and 131,839. These are visits under the file lock, independent of disk timing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An operation-count regression shows sparse flushes avoid repeated traversal of the clean prefix. Sync still covers its starting generation under concurrent writes.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
