---
id: TASK-683
title: >-
  A writer retaking on its own disk with nothing unuploaded can't count its own
  copy: one member's disk lost and the daemon's restart leave the cell
  untakeable without force
status: Done
assignee: []
created_date: '2026-11-20 01:37'
updated_date: '2026-11-20 07:41'
labels:
  - durability
  - peers
dependencies: []
references:
  - crates/local_cell/src/upload.rs
  - crates/local_cell/src/sim/world.rs
priority: medium
type: bug
ordinal: 1064000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-19 by the replicated simulation (TASK-680, profile retakes, seed 7, before the world mirrored the code). Shared::fence counts the old writer's own copy only if the taking host is that writer in the incarnation the head names AND its store holds a write-ahead log (`own = recovering.has_wal()`), because the replica directory's incarnation says nothing of the store directory: a store directory lost while the replica directory survives must not count as the writer's copy. But a writer whose commits were all uploaded holds no log either, so after its daemon restarts it needs f+1 members other than itself, not f: with f = 1 and one member's disk lost before the writer replaced it, the cell can't be taken until an operator forces it (TASK-681), though only one failure happened besides a process restart, which the spec (Take's `self`) tolerates. The fix needs the store directory to say it is the writer's copy in that incarnation (e.g. the replica incarnation and writer epoch recorded in the store's description or StartFile at the take), so an empty log can count.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A writer's store records, at its take, what makes its empty write-ahead log count as its copy in the configuration it activates; a store directory made anew does not
- [x] #2 Shared::fence counts the writer's own copy, empty, from such a store, and only from one
- [x] #3 A test: f = 1, one member's replica directory lost, the writer's daemon restarted with everything uploaded, the take succeeds without force; with the store directory made anew, it is refused
- [x] #4 The simulation's World::members_gone counts as the code does
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed bc20ed4aa + ca9035b25. Design as built: the store description notes writer_copies, the epochs of the configurations it is its writer's copy of; each take of a replicated cell adds its writer's epoch, and, its log restored whole from a quorum of the named configuration's members (not forced), that one's; it keeps those at the named configuration's epoch or later; drop_wal clears them durably before the log goes. holds_own_copy(E) is membership alone (a non-empty log no longer counts by itself: restored in part, or a later writer's, holds records too). A verifying review of the first version found the drop_wal hole and the f = 0 reopen gap, both fixed in ca9035b25 with dropped_logs_are_no_writers_copy (mutant without the clearing caught). The user asked for no further review rounds. Verified: Mac workspace 2816/2816; ubu1 under fxjob clippy + workspace + sim 3128/3129, the one failure TASK-650's SIGABRT at exit (cladefs::tenants tenants_hold_their_own_pages, passing alone); retakes_on_the_writers_disk_count_its_empty_log red with either hand mutant of holds_own_copy.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
A writer's store notes which configurations it is the writer's copy of (cleared when its log is dropped); a retake counts its log, empty or not, only from such a store. Fixes the cell left untakeable after a restart plus one lost member, and closes the dropped-log hole the review found. Mac 2816/2816, ubu1 3128/3129 (TASK-650).
<!-- SECTION:FINAL_SUMMARY:END -->
