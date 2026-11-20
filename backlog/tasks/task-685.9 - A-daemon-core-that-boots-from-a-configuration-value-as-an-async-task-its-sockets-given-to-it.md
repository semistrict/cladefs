---
id: TASK-685.9
title: >-
  A daemon core that boots from a configuration value, as an async task, its
  sockets given to it
status: In Progress
assignee: []
created_date: '2026-11-20 04:01'
updated_date: '2026-11-20 05:40'
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
- [x] #1 cladefsd's main is a thin shell over the core, behaviour unchanged (its tests and ubu1's e2e green)
- [ ] #2 A madsim test boots two cores as nodes over the simulated network and an in-process bucket
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Landed (env per node + core): node_local::env reads the process's environment normally and a simulated node's own under madsim (set_vars; tested under madsim: two nodes apart, a restarted node reads what its restart gives); cladefsd's daemon modules read their configuration through it (bucket, token, roots, stats, replicas, replace-after, part cache, disks, mount, headroom, drain). cladefsd::boot is the core; main keeps what is the process's (logger, signals, panics, failpoints, limits, OTLP) and calls it. DECISION: the 'configuration value' is the node's environment, not a typed Config, so production and the simulation configure a daemon with the same parsing code; boot binds its own sockets in main's order to keep behaviour unchanged (madsim TCP with step 6). Verified: Mac workspace 2814/2814, node_local madsim 4/4; ubu1 under fxjob clippy + workspace 3124/3126, the 2 failures load flakes passing alone (TASK-677; TASK-686 filed). AC #2 (boot two cores as madsim nodes) waits on doc-24's blocking decision (What the thread inventory found).
<!-- SECTION:NOTES:END -->
