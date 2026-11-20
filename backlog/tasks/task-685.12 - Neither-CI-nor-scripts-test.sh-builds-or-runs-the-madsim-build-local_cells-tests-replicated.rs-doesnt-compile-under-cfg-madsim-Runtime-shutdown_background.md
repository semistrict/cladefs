---
id: TASK-685.12
title: >-
  Neither CI nor scripts/test.sh builds or runs the madsim build: local_cell's
  tests/replicated.rs doesn't compile under --cfg madsim
  (Runtime::shutdown_background)
status: In Progress
assignee: []
created_date: '2026-11-20 14:18'
updated_date: '2026-11-20 14:36'
labels: []
dependencies: []
parent_task_id: TASK-685
priority: high
ordinal: 1087000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found 2026-11-20 merging main into PR #2: RUSTFLAGS='--cfg madsim' cargo check -p local_cell --tests fails at crates/local_cell/tests/replicated.rs:119, madsim_tokio's Runtime has no shutdown_background. The madsim tests (node_local, local_cell, bucket_cell tests/madsim.rs) run only by hand. A test.sh part (and CI job) should check the madsim build of every crate it covers and run its tests.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
