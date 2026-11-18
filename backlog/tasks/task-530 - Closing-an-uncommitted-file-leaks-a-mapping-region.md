---
id: TASK-530
title: Closing an uncommitted file leaks a mapping region
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 12:25'
updated_date: '2026-11-18 22:48'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 920000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Audit of revision 845a6c123, finding 15.

Mapped open pops a region at `crates/extent_files/src/file_set.rs:292`. Last close of an uncommitted file removes its record at line 423 without returning its region. Repeated create/close without sync exhausts the finite region pool, leaving later files without mapped reads until reopen. Verify a one-region file set can still map a committed file after creating and closing an empty uncommitted file.

Isolated arm64 macOS validation. Give a mapped file set one region. Create, write and sync a file and fetch its first page. Repeat after opening then closing an uncommitted empty file. The control returned a mapped page. With the empty-file close, the same fetch returned no mapping.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 In a one-region file set, closing an empty uncommitted file leaves the next committed file eligible for a mapped fetch.
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-18: taken up by the Codebase Bug and Performance Audit session, in one batch of file-set fixes with TASK-551, 552, 553, 530, 583 and 584 (branch t3/codebase-bug-perf-review).

Already fixed on main by b1f1e1c7a (close_file's last close of an uncommitted file goes through remove_file, which releases the region). Checked 2026-11-18: sqlite_extents' closing_an_uncommitted_file_returns_its_mapping_region passes, and fails with close_file dropping the record without remove_file.
<!-- SECTION:NOTES:END -->
