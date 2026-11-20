---
id: TASK-685.8
title: Full-stack simulation in CI and a nightly soak
status: Done
assignee: []
created_date: '2026-11-20 03:34'
updated_date: '2026-11-20 19:33'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1074000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 8: a few hundred seeds per profile on each push, a soak at night, failing seeds kept as regression seeds.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 CI runs the profiles; the nightly runs the soak; a failing seed is reported with its replay command
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Done: CI madsim part 200 seeds a push (each a process); nightly.yml madsim-soak 2,000 seeds from a date-made base; failures print MADSIM_TEST_SEED=SEED replays; crates/cladefs/tests/madsim-seeds holds regression seeds, run first.
<!-- SECTION:NOTES:END -->
