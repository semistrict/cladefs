---
id: TASK-484
title: >-
  Reads of sealed extents skip extent_store's range checks: a misaligned or
  empty READ answers unlike an open extent's
status: To Do
assignee: []
created_date: '2026-11-16 06:20'
labels:
  - bug
dependencies: []
priority: low
ordinal: 882000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-470 (what the real store answers for a range). cladefs's ExtentStore::read (crates/cladefs/src/extents.rs) serves a sealed extent through read_cached, which checks only that the range ends within MAX_EXTENT: a misaligned READ of a sealed extent succeeds, and one of no bytes returns Ok; the same READ of an open extent, or of one the store doesn't hold, reaches extent_store's check_io and fails EINVAL (Unaligned), so a zero-length READ of a missing extent says EINVAL, not ENOENT. doc-4 says READ offsets and lengths are multiples of block_size (misaligned is EINVAL) and says nothing of zero lengths. The virtio device refuses misaligned ranges before the store since TASK-470 (protocol::Bounds), but other callers of ExtentStore::read (the daemon's clients, FUSE, tests) still see the difference, and doc-4 should say what a READ of no bytes answers.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 ExtentStore::read answers a misaligned range EINVAL whether the extent is sealed, open or missing
- [ ] #2 doc-4 says what a READ or WRITE of no bytes answers, and the store answers it so for sealed, open and missing extents alike, with a test of each
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
