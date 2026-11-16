---
id: TASK-487
title: >-
  Neither the simulation nor specs/bucket sees what a host remembers of a
  branch, nor cells' holding: a stale remembered cell opened a superseded host
  (TASK-449's review)
status: To Do
assignee: []
created_date: '2026-11-16 07:46'
labels:
  - sim
  - tla
  - review-2026-11-16
dependencies: []
priority: medium
ordinal: 883000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
TASK-449 made a host remember what its fork of a point left of the clone (extent_remote::known::Learned: record, cell object, no head) for the clone's first open (extent_store upload.rs first_reads). Its review found a stale remembered cell let a host superseded since by a later placement open the clone (fixed: the cell read once the head is taken, Taking::remembered, cell::still_held; test restores_refused_to_placements_superseded_since_their_forks). Neither model could have found it: the extent_store simulation never forks points through Remote::forked, so it never learns anything (it passes no learning to Shared::run), and specs/bucket has no cells: its placements give tokens per branch (Place), assuming a token current as a head is taken, which the code realises by the cell's holding. doc-7 (What a host knows) argues the fix instead of checking it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The simulation restores points through the host's fork path, its clones' first opens taking what the fork left, and a later placement holding a clone's cell between fork and open, the open refused
- [ ] #2 A hand mutant that skips the read after the take (Taking::remembered None) fails the simulation
- [ ] #3 specs/bucket models cells' holding as the token's source, or doc-7 says why it need not, its argument checked against a model under 10 minutes
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
