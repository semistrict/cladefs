---
id: TASK-703
title: A killed simulated host's flocks outlive it
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 20:03'
updated_date: '2026-11-20 20:04'
labels:
  - bug
  - madsim
dependencies: []
priority: high
ordinal: 1108000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the madsim replicated attrition profile (TASK-685.7): a host killed and started again failed its boot, 'CLADEFSD_PART_CACHE: ... another cache has it'. A node's values are leaked at its kill (node_local), so its part cache's flock, and its stores' directories' (Dir::take's TAKEN), are never let go, where a process's death lets its locks go. Fix: node_local::Flock, an exclusive lock that is flock built normally and, under madsim, a lock table of the simulation's, held per node, which a node's restart clears; the part cache and Dir::take hold one.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
