---
id: TASK-685.1
title: >-
  Spike: cladefs under --cfg madsim, two simulated daemons, a cell made on one
  and taken on the other
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 03:32'
updated_date: '2026-11-20 03:34'
labels:
  - testing
  - simulation
dependencies: []
parent_task_id: TASK-685
ordinal: 1067000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
doc-24 step 1. Build the workspace with RUSTFLAGS="--cfg madsim" and tokio/tonic renamed to madsim-tokio/madsim-tonic, boot two daemons as madsim nodes over an in-process bucket, make a cell on one and take it on the other. List every break (threads, runtimes, block_in_place, Unix sockets, statics, std::fs, crates that use real tokio internally) with counts; size the later steps by it; if madsim can't host cladefsd, say why and rewrite doc-24 for the in-house fallback.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A written result in doc-24 (What the spike found) with the list of breaks and the decision to go on with madsim or not
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
