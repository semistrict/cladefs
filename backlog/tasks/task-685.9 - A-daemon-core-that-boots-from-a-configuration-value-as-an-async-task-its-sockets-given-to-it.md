---
id: TASK-685.9
title: >-
  A daemon core that boots from a configuration value, as an async task, its
  sockets given to it
status: To Do
assignee: []
created_date: '2026-11-20 04:01'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1075000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 2, added by the spike (TASK-685.1): cladefsd's main reads its configuration from the environment and process statics, spawns threads, parks the main thread and binds Unix sockets, none of which a madsim node can do. A core that takes a configuration value and its listeners (control, peers, metrics; TCP under madsim) and runs as an async task; main reads the environment and calls it; the simulation calls it once per node.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 cladefsd's main is a thin shell over the core, behaviour unchanged (its tests and ubu1's e2e green)
- [ ] #2 A madsim test boots two cores as nodes over the simulated network and an in-process bucket
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
