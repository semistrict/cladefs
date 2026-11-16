---
id: TASK-481
title: >-
  A checkpoint table lookup searched branch-first over entries sorted
  sequence-first
status: To Do
assignee: []
created_date: '2026-11-16 04:50'
labels:
  - bug
  - bucket
dependencies: []
ordinal: 879000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the TASK-474 rename (replacing extent_frame::table::Id = (u64, u64) with storage_device::ExtentId): table entries come from imbl::OrdMap<ExtentId, _>, so they were stored in ExtentId's sequence-first order, while Footer::block (now TableFooter::block) searched its block index as if branch-first tuples. In a checkpoint holding several branches over more than one 64 KiB block, a lookup could pick the wrong block and miss an extent. The rename made both one type and one order, bytes unchanged, which fixes it, but no test covers the case; per AGENTS.md, every bug is a blind spot: close it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test builds a table of extents of several branches spanning more than one block and looks up every entry; it fails with the old branch-first search put back (a mutant in crates/*/mutants)
- [ ] #2 The table's doc says its entries are in ExtentId's order and lookups search that order
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
