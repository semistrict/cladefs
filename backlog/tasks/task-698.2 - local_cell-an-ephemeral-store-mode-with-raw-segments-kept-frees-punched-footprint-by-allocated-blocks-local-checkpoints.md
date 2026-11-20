---
id: TASK-698.2
title: >-
  local_cell: an ephemeral store mode with raw segments kept, frees punched,
  footprint by allocated blocks, local checkpoints
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 16:21'
updated_date: '2026-11-20 17:53'
labels: []
dependencies: []
documentation:
  - >-
    backlog/docs/doc-25 -
    Ephemeral-cells-host-local-children-of-a-version-migrated-live.md
parent_task_id: TASK-698
ordinal: 1091000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
An ephemeral cell has no bucket. Its segments stay raw in its directory, no uploader, framing or lag bound runs, and freed space must come back without compaction. It must recover after a crash from a local checkpoint plus the logs after it (doc-25, The cell).
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 An ephemeral store commits, closes segments and reads its own extents with no bucket configured, and starts no uploader or framing
- [ ] #2 A free that is Local punches its extent bytes out of the raw segment; a segment emptied is unlinked; a test shows st_blocks shrinking
- [ ] #3 Its footprint and admission count allocated blocks, so a punched hole returns space to the tenant allotment
- [ ] #4 Recovery from the latest local checkpoint plus later logs gives every Local commit, proven by crash tests (shuttle/madsim) at every step of checkpointing
- [ ] #5 A drain writes a local checkpoint of every ephemeral cell
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. File::punch and File::allocated through every file layer (host fallocate/F_PUNCHHOLE, memory, page cache, metered, raw); footprints count kept segments by blocks allocated.
2. Segments: Raw (closed, never framed, log beside it) and Kept (base covers it, raw file in kept/) phases; ephemeral segments never looked at ahead; reaping lets kept ones go; runs let go of in kept segments queued to give back.
3. cell/ephemeral.rs: Base (state over the parent version: own extents, parent extents freed, root, sequence, spent, capacity, kept segments with log headers, runs last given back, keys blob), settle before WAL recovery, Recovering::open_ephemeral (parent state from the opener), LocalCell::make_ephemeral, checkpoint (covers durable closed segments, syncs raw files, writes base, links into kept/ then unlinks, moves WAL start), give_back.
4. Recovering::open refactored to share LocalCell::assemble.
5. Tests: checkpoints, give-back of whole segments and single runs, closing covers all, crashes at every disk operation through two checkpoints.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-11-20 batch 1 landed locally: design changed from doc-25's first draft in two places, both found by the crash test: raw runs a flush held only in log records are written back into raw files at open (Recovered::restore_raw) and synced before a checkpoint drops their logs; segments move to kept/ by link, sync, unlink, as a crash keeps directories' entries apart. Base stores only what the cell holds over its parent's version, not the parent's map. local_cell: 382/382 tests pass on macOS; clippy (repo flags, shuttle too) clean. Not yet: Linux build of DiskFile::punch (fallocate), footprint test of kept holes on a host dir.
<!-- SECTION:NOTES:END -->
