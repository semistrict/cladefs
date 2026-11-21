---
id: TASK-718
title: >-
  madsim ephemeral_cells_survive_crashes_and_migrations hangs on seed 9: no
  simulated progress, 8 s of CPU in 80 min
status: To Do
assignee: []
created_date: '2026-11-21 01:29'
labels: []
dependencies: []
priority: high
type: bug
ordinal: 1120000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
On ubu1 (fxjob mainchk, tree ~/src/fxdax-nodelocal2 at a5f445bbd), MADSIM_TEST_SEED=9 of the madsim test ephemeral_cells hung: its last lines at 18:57, ephemeral cells at host1 and 503s seeded on head gets (head/0..9 of cell 000bc3v6g0000), then 'has been running for over 60 seconds'; the process idle (8 s CPU) until stopped at 20:35. A simulation that stops making progress should fail as a violation (130e555cf), not hang. Log kept: ubu1:/mnt/fxdax-scratch/mainchk-seed9-hang.log. Stopped by the pgbench investigation (TASK-707) with the user's OK, as it held the bench.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 MADSIM_TEST_SEED=9 of ephemeral_cells finishes, or fails naming what it waited for, in under a minute
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 scripts/upstream-changes.py --check passes
- [ ] #2 Deviations from upstream documented where they occur
<!-- DOD:END -->
