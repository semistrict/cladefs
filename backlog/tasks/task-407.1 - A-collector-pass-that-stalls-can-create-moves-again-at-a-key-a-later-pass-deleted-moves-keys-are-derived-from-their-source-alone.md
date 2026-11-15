---
id: TASK-407.1
title: >-
  A collector pass that stalls can create moves again at a key a later pass
  deleted: moves keys are derived from their source alone
status: Done
assignee:
  - '@claude'
created_date: '2026-11-14 22:58'
updated_date: '2026-11-15 06:33'
labels: []
dependencies: []
parent_task_id: TASK-407
priority: high
type: bug
ordinal: 798000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
collector::compact_object GETs the source object, then creates moves/<source> create-only. A pass that read the source, then stalls (fenced: a later pass took the cell's collector lease), can create moves/<source> after later passes compacted the source, moved its moves on (moves/moves/<source>) and deleted moves/<source>: the create succeeds at a freed key, with the live set as that stale pass saw it, other bytes than the object readers and caches knew at that key. specs/bucket misses it: compaction there is atomic with the pass's last step. Found reading the code for TASK-407 (2026-11-14). Fix within TASK-407: a moves key names its pass too (the cell's collector epoch), so that two passes never write one key; readers and writers find the moves of a source by listing its prefix.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 specs/bucket models compaction's create landing after its pass's step, and a mutation keying moves by source alone finds a key written twice
- [x] #2 Moves keys carry the pass's epoch; readers and take_moves find them by listing the source's moves
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [x] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [x] #2 scripts/upstream-changes.py --check passes
- [x] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Spec: moves keyed by [b, s, e, way of passes], creates in flight (GcLand, create-only), readers and the collector find moves by listing; NoMovesByPass finds a key written twice in 1 s (4 passes). Code to do.

Code pushed c8344a37a: moves keys <C>moves/<rest>/<epoch> (layout::moves(key, pass), moves_of, moves_under; Kind::Moves names its pass); fetch lists an object's moves and follows the latest pass's holding the extent; MovesMaps merges in pass order; collector keeps every pass's moves its ways reach. Tests passes_creating_moves_late_create_them_at_keys_of_their_own / _need_moves_by_pass (a stalled pass's create after later passes compacted and deleted: key never written vs written twice with the rule off). macOS gate: clippy + workspace 2493/2493.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Moves keyed by their pass (c8344a37a), with the fork-point fix the simulation found (3f500554b: a pass reads a kept fork point by its checkpoint's key). Spec: NoMovesByPass. Code tests: passes_creating_moves_late_create_them_at_keys_of_their_own / _need_moves_by_pass. Verified: macOS clippy + workspace 2493/2493 and sim part 50/50; ubu1 (e015d0fe, includes both) workspace 2791/2792 (TASK-315 only), Shuttle passing.
<!-- SECTION:FINAL_SUMMARY:END -->
