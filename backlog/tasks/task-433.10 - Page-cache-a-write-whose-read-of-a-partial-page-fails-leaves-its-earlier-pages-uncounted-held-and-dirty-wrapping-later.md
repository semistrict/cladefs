---
id: TASK-433.10
title: >-
  Page cache: a write whose read of a partial page fails leaves its earlier
  pages uncounted, held and dirty wrapping later
status: Done
assignee:
  - '@claude'
created_date: '2026-11-18 03:21'
updated_date: '2026-11-18 04:41'
labels: []
dependencies: []
parent_task_id: TASK-433
priority: medium
type: bug
ordinal: 899000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the review of TASK-433.9's page frames. Cached::write_at (crates/extent_store/src/pages.rs) inserts and dirties pages as it goes, and returns at once if reading what the file holds of a partial page fails: the pages it inserted and dirtied before are never added to the cache's held and dirty counts, nor the file's size and dirtied time set. Their eviction, write-back or drop later subtracts them, wrapping held and dirty around u64: the governor then sees a cache of ~2^64 bytes, and the flusher writes back forever.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A write whose read of a partial page fails counts every page it inserted and dirtied before the failure, and returns the failure
- [x] #2 A test with a file failing reads shows held and dirty exact after such a write, and after the pages are evicted
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Capture the read's failure, stop the loop, count what was inserted and dirtied, set size to what was written, return the failure; a test with a file whose reads fail.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Verified: pages::tests::a_write_failing_its_read_counts_what_it_held (hand mutant, the early return, fails it: held 0 against 8192); Mac workspace 2659/2659, ubu1 workspace 2973/2973.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Cached::write_at returned at once when reading a partial page failed, its earlier pages never counted: held and dirty wrapped as they went. It now counts them, sets the size to what it wrote, and returns the failure. Tested with a file failing reads; Mac and ubu1 suites green.
<!-- SECTION:FINAL_SUMMARY:END -->
