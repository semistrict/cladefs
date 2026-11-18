---
id: TASK-515
title: 'One object a cell: the head holds the record and the cell''s object'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-18 07:02'
updated_date: '2026-11-18 08:06'
labels: []
dependencies: []
ordinal: 905000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
A cell is one log now (PR #1), so its three swapped objects say one thing three ways: the record (what made it, owner, deleted, resumed, DEKs, inherited DEKs), the cell's object (placement epoch, holder, token, kind, KEKs, shredded) and the head (writer's epoch, holder, token, end, runs, checkpoint, versions). A placement's taking and a writer's taking are the same taking. Fold the record and the cell's object into the head: one key, {prefix}head; one conditional write takes a cell; a deletion is one swap to the final epoch; a fork creates the child's head (Forking, epoch 0), refers, checks the version, swaps it to Clone; a key rotation rewraps in one swap. The collector and a writer's taking read one object. Delete record.rs, cell.rs's Object and its takings, layout::record and layout::cell, the record/head interleavings in specs/bucket.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 layout has one per-cell swapped key, the head; no record or cell object key
- [ ] #2 A writer's taking is one conditional write of the head, with the host's placement token
- [ ] #3 Deletion, fork, resume, abandon, key rotation and shredding each swap the head
- [ ] #4 specs/bucket models one head per cell; every model holds and every mutation is caught
- [ ] #5 bucket_cell, local_cell (sim, Shuttle), cladefs (Mac and ubu1 Linux), guest suites pass
- [ ] #6 Net lines removed, reported by src/tests/TLA+
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
1. layout::Head gains the record's and the cell object's fields: format (first), owner, made (record::Kind: Root, Own, Forking, Clone), resumed, kind (fxfs/sqlite/memory), keks, shredded, deks, inherited; epoch 0 = made, never taken; FINAL = deleted. layout::record and layout::cell go.
2. head.rs owns every transition of the head: take (making a Root at epoch 1 with its KEK if none; refusing FINAL, shredded, Forking, a higher token), swap (the writer's), end (deletion: one swap to FINAL, Own made if none), fork create (Forking, epoch 0, keks and inherited DEKs from the pool), finish (Clone), resume, abandon, shred, rotate (the holder's swap), give DEKs (in the taking's write).
3. record.rs keeps Kind and the predicates on Head (writable, removable, retires_unmarked, is_child_of); cell.rs keeps cell::Kind and listing; Object, Held, take/hold/still_held/forget/delete go. A placement's token is the head's token: no cell epoch.
4. Callers: keys.rs, writer.rs (one write takes), collector.rs (one GET), recovery, sweep, gc bins, testing; local_cell upload/sim; cladefs host_cell, open_cell, kept, tests.
5. specs/bucket: rec folds into head; deletion one step; forks' child head; rerun every model and mutation.
6. Tests: bucket_cell, local_cell sim + Shuttle, cladefs Mac, ubu1 Linux + guest. Report net lines.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Folded: layout::Head holds what the record and the cell's object held (FORMAT 19); record.rs is Made and the head's predicates; cell.rs kinds and listing; head::take one conditional write giving missing keys (keys::taking); deletion one swap; forks make the child's head with its keys; makers create roots (keys::make, KEK first, refused tenants get nothing), a taking never makes a cell (the sim found a removed cell's ID re-made as a root otherwise). specs/bucket: rec folded into head, staleTake/OpenGone gone; 18 models hold, 30 mutations caught. Restores 9 requests in 9 round trips (were 14 in 13). Sim searches for candidates_first and references_held find no seed now; TLA mutations cover them.
<!-- SECTION:NOTES:END -->
