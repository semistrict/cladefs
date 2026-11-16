---
id: TASK-149.48
title: >-
  Simulation: deletions of forks' children before their records, and resumes
  racing a live restore
status: To Do
assignee: []
created_date: '2026-11-16 03:55'
labels:
  - sim
  - testing
  - review-2026-11-15
dependencies: []
parent_task_id: TASK-149
type: task
ordinal: 879000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-455's review found two bugs the simulation never reached (extent_store's sim with SIM_OFF=fences_kept or resumes_marked finds no seed of 0..20,000 of children() or restores()): a fork's child deleted before its parent's writer records it, its deletion stopped between ending the head and writing the record while a pass runs; and a fork of a point resumed by another host (a store opening the clone, branch::resume_point_fork) while the restore's own host is alive and slow, writing its reference after the clone was made, deleted and its cell removed. The direct tests (extent_remote's deletions_fences_need_fences_kept, late_references_of_resumed_clones_need_resumes_marked) and specs/bucket's NoFencesKept and NoResumesMarked catch them; the simulation, the class oracle, generates neither interleaving.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The simulation deletes forks' children before their records land (the TASK-79.78 race), passes running between the deletion's head and its record
- [ ] #2 The simulation resumes clones being made from a second host while the restore's host is alive and stalled before its reference
- [ ] #3 test_needs for fences_kept and test_leaks for resumes_marked find a seed of 0..20,000 and are added to sim/mod.rs
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
