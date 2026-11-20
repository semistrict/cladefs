---
id: TASK-697
title: >-
  Moves of moves naming nothing live stay while their parent is kept only for a
  fork's ways from seals
status: To Do
assignee: []
created_date: '2026-11-20 15:58'
labels:
  - collector
  - garbage
dependencies: []
priority: low
type: bug
ordinal: 1083000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen in local_cell's sim::restores_racing_for_one_child, seed 3, on TASK-691's head slots (two-slot heads in the simulation): a pass compacts moves 6 of segment 3.3 into moves/moves/.../6/8; moves 6 stays, a fork's inherited (behind) state keeping every moves on its extents' ways from where they were sealed (rule ways_from_seals, collector hops); moves 8 names no extent any state names, but an unnamed moves goes only once what it is of is gone (specs/bucket GcFinish doomedKey, collector fates). So 8 stays for as long as the fork's state does: space leaked, nothing lost. specs/bucket allows the same state. The simulation's garbage check (testing::uncollected) now mirrors that rule, keeping unnamed moves of a kept parent, citing this task; deciding whether the collector may delete such moves (a writer's state not yet checkpointed may still follow 6 to 8 for an extent 8 names) needs the spec first.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 specs/bucket says whether a pass may delete moves naming no extent any state it read names, their parent kept only as a hop of a behind state; the model and a mutation show it
- [ ] #2 If it may, the collector does, and testing::uncollected stops keeping unnamed moves of a kept parent
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
