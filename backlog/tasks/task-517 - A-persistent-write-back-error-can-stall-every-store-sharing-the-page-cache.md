---
id: TASK-517
title: A persistent write-back error can stall every store sharing the page cache
status: To Do
assignee: []
created_date: '2026-11-18 12:23'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 907000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 2.

In `crates/local_cell/src/pages.rs:372`, writers wait until the shared dirty count falls below the hard limit. Failed writes retain dirty pages at line 719, and the live flusher logs the error at line 425. If the failing file alone exceeds the bound, a writer targeting a healthy file can wait indefinitely. Give admission a way to return the write-back failure while preserving dirty data and the memory bound. Exercise a healthy writer with a different file failing above the bound.

Isolated arm64 macOS validation. Use the cache's real flusher with a failing in-memory file. Set the hard limit to 8 KiB and dirty 16 KiB. Start a writer on a healthy second file, then clear the fault. The healthy writer was still waiting after 250 ms with 16 KiB dirty. It completed after the other file resumed accepting writes. An unsuccessful flush also notifies blocked writers, which wake the flusher again. This is retry churn under persistent I/O error.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 With one failing file above the shared hard limit, a healthy writer makes bounded progress or receives a meaningful error. Dirty data and cache limits are preserved, and retry work stays bounded.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
