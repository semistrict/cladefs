---
id: TASK-411.8
title: >-
  Open segments refuse writes that are not whole 4 KiB sectors; extent_store
  simulation writes 4 KiB blocks
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-14 19:07'
updated_date: '2026-11-14 21:59'
labels: []
dependencies: []
parent_task_id: TASK-411
type: task
ordinal: 805000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Encrypting open segments with AES-256-XTS per 4 KiB sector (TASK-411.5, doc-16) needs no read-modify-write only if every write reaching an open segment is whole sectors. A probe on 2026-11-14 (an assert in Segments::reserve and Segments::append) found every real path whole sectors on macOS: 574 tests of extent_store, sqlite_extents and fxdax passed; only the 62 of extent_store's simulation failed, as sim/world.rs writes 512-byte blocks. Nothing enforces it today.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A write or seal reaching an open segment whose length or offset is not a multiple of 4096 is refused with an error naming it, not written
- [ ] #2 extent_store's simulation writes 4 KiB blocks, and its tests pass
- [ ] #3 A test shows a misaligned write refused
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
812390bdf (macOS): Unsectored, refused before anything is written, checked in two places: Store::begin_write_locked, for writes going to segments, on the extent offset and length, before the extent opens, so that the seal (which copies the extent whole, from 0) appends whole sectors too; and Segments::reserve/append, on the segment offset and length, which is what holds for every path, seals included. SECTOR (segments.rs) is 4096. write_with writes whole-sector pieces. fxdax maps Unsectored to EINVAL. Large extents (own raw files) are not checked: no open segment. The sim's BLOCK is SECTOR. Its seeds did not move: the first seed finding the case was the same at 512 and 4096 for takeovers_need_fences (378), deletions_need_records_reread (85,424) and recoveries_need_newest_checkpoints (11,020). The last two are slow at both sizes; the comment on the third names seed 1,389, which no longer finds the case even at 512. Tests: extent_store 268, sqlite_extents 22, fxdax 348, all passed (2576 s on a loaded Mac).
<!-- SECTION:NOTES:END -->
