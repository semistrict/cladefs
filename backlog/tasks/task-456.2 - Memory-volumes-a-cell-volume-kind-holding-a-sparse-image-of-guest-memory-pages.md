---
id: TASK-456.2
title: >-
  Memory volumes: a cell volume kind holding a sparse image of guest memory
  pages
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 13:33'
updated_date: '2026-11-15 22:41'
labels:
  - memory
  - host
dependencies:
  - TASK-456.1
parent_task_id: TASK-456
priority: high
ordinal: 863000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A third cell::Kind beside fxfs and sqlite (extent_remote/src/cell.rs): a store whose root names one page tree, the image of a VM's memory, no filesystem. Read by page; written by page or as holes, a batch committed at once; forked as any branch, so a restored VM's next snapshot is a fork of its parent's memory with only what changed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A memory volume is made in a cell, listed in the cell's object with kind memory, opened through extentd by the tenant whose cell it is and no other, shown by tests
- [x] #2 A batch of pages and holes is committed in one commit; a crash before it leaves the image as the last commit had it, shown by a crash test
- [x] #3 A fork of a memory volume reads the parent's pages until it writes its own, and stores only what it wrote, shown by a test counting the fork's own extents
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. extent_files: holes written: a file's dirty chunks are data or holes (punch of whole chunks); a commit places a hole as no ref, leaving the block it replaces; reads of it zeros.
2. extent_remote::cell::Kind::Memory ('memory'); extentd: wire OPEN_MEMORY beside OPEN_SQLITE (wire version bump), codec, control API VolumeKind::MEMORY, client open_memory; Go bindings regenerated.
3. cladefs::memory_volume: a memory volume opened through the daemon (as sqlite_extents::daemon opens SQLite's): an extent_files volume of one file, 'memory', of the guest memory's size; write pages and holes, commit them at once, read pages, the runs of pages a window maps.
4. Tests: kind round trip and cell listing; tenant isolation (another tenant's open fails EACCES, as stores'); a batch of pages and holes committed once and a crash test with FakeDevice crashes (the image is the last commit's); a fork reads its parent's pages and stores only what it wrote (its own extents counted).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Implemented: extent_files punch (holes stored as nothing, reads zeros); extent_remote::cell::Kind::Memory ('memory'); extentd OPEN_MEMORY (wire version 8, fingerprint 0x8f98a3970017c8f1), VolumeKind::MEMORY in the control API, DaemonConnection::open_memory, Go bindings regenerated; cladefs::memory_volume::MemoryVolume (open/write/punch/commit/read/fork/extents/close) over the daemon's store. Tests: extentd stores_made_are_volumes_of_their_cells_of_their_kinds (memory listed in its cell), memory_volumes_commit_pages_and_holes_and_forks_store_only_their_own (reopen reads back; a fork reads the parent's pages and holds one extent of its own), grants tenants_reach_no_memory_volume_of_another_tenants_cell (EACCES), extent_files crash a_crash_leaves_the_memory_as_a_commit_made_it (30 commits of pages and holes, a crash at every barrier), pages punched_chunks_are_holes_stored_as_nothing and punches in the random model (600 seeds once). Validation (branch t456-files, 4ff3b46b): macOS CLADEFS_TEST_PARTS='clippy workspace go' scripts/test.sh: clippy clean, 2505 passed, Go bindings checked; ubu1 (fxjob) clippy + workspace: 2807 passed; scripts/upstream-changes.py --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Memory volumes: a third cell volume kind holding a VM's memory as an extent_files volume of one file, zero pages as holes that store nothing, a batch of pages and holes one commit; made through extentd's OPEN_MEMORY by the tenant whose cell it is and no other; forks read the parent's pages and store only their own. Verified by daemon, grants, crash and model tests and the full suites on macOS (2505) and Linux on ubu1 (2807).
<!-- SECTION:FINAL_SUMMARY:END -->
