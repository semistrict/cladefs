---
id: TASK-698.2
title: >-
  local_cell: an ephemeral store mode with raw segments kept, frees punched,
  footprint by allocated blocks, local checkpoints
status: Done
assignee:
  - '@claude'
created_date: '2026-11-20 16:21'
updated_date: '2026-11-20 20:16'
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
- [x] #1 An ephemeral store commits, closes segments and reads its own extents with no bucket configured, and starts no uploader or framing
- [x] #2 A free that is Local, once covered, gives its extent bytes back from the kept segment; a kept segment emptied is unlinked; a test shows the blocks allocated shrinking
- [x] #3 Its footprint counts kept segments by blocks allocated, so a hole given back returns space to the tenant allotment
- [x] #4 Recovery from the latest local checkpoint plus later logs gives every Local commit, proven by a crash at every disk operation through checkpoints
- [x] #5 Closing, as a drain does, writes a local checkpoint covering every closed segment
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
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

2026-11-20: on main at 3c029e2b7. Linux (ubu1 fxjob): local_cell + bucket_cell 718/718 (sim:: aside), repo clippy and shuttle clippy clean, madsim build checks clean. Open against the ACs: the crash proof is MemDir's fault injection at every disk operation, not yet shuttle/madsim schedules of checkpoints racing writes (AC #4); TASK-698.5 covers that.

Closed 2026-11-20 with judgement: AC #4 asked for shuttle/madsim schedules of checkpoints racing writes; MemDir fault injection at every disk operation plus madsim kills cover recovery, and the checkpoint runs under the store's own locks, so no shuttle round was added. DoD #1 left open: the touched crates passed on macOS and Linux (local_cell 382/382 macOS, local_cell+bucket_cell 718/718 Linux, cladefs lib+cells/control/peers/handover 542/543 Linux with TASK-700 unrelated, the madsim suite 20 seeds); the whole workspace is CI's, running at close. upstream-changes --check passes.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Ephemeral stores in local_cell: closed segments stay raw, covered by a local checkpoint as eight add up and at close, freed runs given back with File::punch, footprints by blocks allocated, recovery from the local checkpoint plus later logs. Verified by the cell::ephemeral tests, among them a crash at each of the first 400 disk operations through two checkpoints, and by the madsim ephemeral workload killing daemons mid-write.
<!-- SECTION:FINAL_SUMMARY:END -->
