---
id: TASK-456.1
title: 'A page tree shared by SQLite files and memory volumes, read lazily'
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 13:33'
updated_date: '2026-11-15 22:41'
labels:
  - memory
  - sqlite
dependencies:
  - TASK-440
parent_task_id: TASK-456
priority: high
ordinal: 862000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
sqlite_extents/src/pages.rs keeps a file's bytes as 4 KiB chunks over write-once extents in a radix tree (fanout 170), holes reading as zeros, copy-on-write commits, compaction of half-dead extents. A memory volume needs the same tree for images of up to 64 GiB (16.7M chunks), which Pages::open's eager walk and per-chunk locations can't serve (TASK-440). One lazy tree for both.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 The page tree is a module both sqlite_extents and memory volumes use, its format unchanged for SQLite files or changed with a format bump (no migration, AGENTS.md)
- [x] #2 TASK-440's criteria hold for it: no node read at opening, memory growing with what is touched, compaction from live counts kept on the device
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Decided with the user 2026-11-15: a shared volume crate.
1. New crate extent_files: format.rs, pages.rs, window.rs and a Volume of named files (open, open_mapped, files, open_file/close_file, read/write/truncate/size, sync, delete, exists, fetch/unfetch, extents, close, check), moved from sqlite_extents with their tests; no SQLite lock levels, no in-memory journals.
2. sqlite_extents keeps its VFS, locks and journals: its Volume wraps an extent_files::Volume (the same name and calls for its users), lock_file/unlock_file/reserved/memory_file its own; unlock commits through the inner volume's sync, as before, skipped once the volume failed.
3. Root format unchanged (TASK-440's version 3); magic stays.
4. Validation: extent_files and sqlite_extents suites on macOS, clippy; ubu1 sqlite daemon tests; then the workspace parts once.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
extent_files holds the volume, page tree, format and window (moved from sqlite_extents with their tests); sqlite_extents keeps its VFS, lock levels and in-memory journals on it, its Volume the same calls for its users; memory volumes (TASK-456.2, cladefs::memory_volume) use the same Volume. Validation (branch t456-files, 4ff3b46b): macOS CLADEFS_TEST_PARTS='clippy workspace go' scripts/test.sh: clippy clean, 2505 passed, Go bindings checked; ubu1 (fxjob) clippy + workspace: 2807 passed; scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
One page tree and volume for SQLite files and VMs' memory: crate extent_files (volume, lazy page tree of TASK-440, format, window), used by sqlite_extents (VFS, locks, journals on top) and by cladefs::memory_volume. Verified by the moved suites and the full suites on macOS (2505) and Linux on ubu1 (2807).
<!-- SECTION:FINAL_SUMMARY:END -->
