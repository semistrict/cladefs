---
id: TASK-407.2
title: >-
  Code: the branch's head replaces its lease; keys name their writer's epoch; a
  commit is its PUTs then the head's swap
status: Done
assignee:
  - '@claude'
created_date: '2026-11-15 01:25'
updated_date: '2026-11-15 06:33'
labels: []
dependencies: []
parent_task_id: TASK-407
priority: high
ordinal: 799000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Implements specs/bucket's head (TASK-407, commit 471b009b1 and after): layout's segment and checkpoint keys name the writer's epoch (FORMAT bump, no migration: no buckets exist); branch's lease object becomes the head [epoch, token, holder, end, chain (runs of epochs), checkpoint], swapped with If-Match; Writer::acknowledge swaps it after the PUTs land, an ambiguous swap resolved by reading it back; takeover swaps it to the next epoch and recovers from it, no fence segment, no gaps; recovery reads the head, its checkpoint and the segments it names; deletion swaps it to the final epoch; WAL rebase by the keys the head names. Removed: fences, gaps, void and late segments, State::fences/strays, forks_after_lease's lease read, newest_checkpoint listing for a branch's own recovery, the Rules fields of rules gone from the spec.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Writers commit by PUT then head swap, takeover and deletion by the head; the simulation's profiles and fault tests pass against it
- [x] #2 The Rules fields of the rules the spec dropped are gone, with their code paths; the rest match the spec's rule names
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Coded (commits 76ddbcd37, 83f62ed06 and the fxdax one after): head replaces lease; segment/checkpoint keys epoch-first (front-coding); FORMAT 10; Footer/Chain lose 'after'; WAL rebase by commits plus held fork-only segments (store restamps in memory only), spec aligned. Behaviour changes in fxdax tests: a fork whose segment landed but whose head swap did not never stands (its deletion writes the child's record deleted from the first, Kind::Fork); orphans arise only after the swap (Dies at writer::acknowledged); a durable commit is create + update (2 round trips); a take reads the head once and swaps it, no listing, its recovery re-read of the head dropped (only the taken head's segments are read). doc-14 'forks stand once their segment lands' to update under 407.4/407.5.

Pushed to main 0a00dd6dc (squashed 1875ee2a2, merged with main's TASK-411 as 82974c2ea: FORMAT 11, rings threaded through recover_from/recover_on/the writer/deletion). macOS gate: clippy + workspace 2490/2490. extent_store sim (dev profile): every holds profile passes after the fork-point fix (the pass keeps checkpoint(parent, at, epoch_of(at)), which cross-cell children read by key; test fork_points_to_other_cells_keep_their_checkpoints_past_later_writers); three needed-rule searches find no seed: TASK-424. Pending: ubu1 Linux run, Shuttle, TASK-424.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
The branch's head replaces its lease (pushed 0a00dd6dc, fixes 19614af3f, 8975f1495, 3f500554b): keys name their writer's epoch, epoch first; a commit is its PUTs then the head's swap; takeover and deletion swap the head; recovery reads what the head names; WAL rebase by commits; 17 Rules fields and their code gone. Verified: macOS clippy + workspace 2493/2493, sim part 50/50; ubu1 (e015d0fe) workspace 2791/2792 (only TASK-315's ZFS page-cache test), Shuttle all passing; upstream-changes.py --check clean.
<!-- SECTION:FINAL_SUMMARY:END -->
