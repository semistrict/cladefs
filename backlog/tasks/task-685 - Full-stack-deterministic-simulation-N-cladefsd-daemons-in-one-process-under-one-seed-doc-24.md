---
id: TASK-685
title: >-
  Full-stack deterministic simulation: N cladefsd daemons in one process under
  one seed (doc-24)
status: To Do
assignee: []
created_date: '2026-11-20 03:31'
labels:
  - testing
  - simulation
dependencies: []
references:
  - >-
    backlog/docs/doc-24 -
    Full-stack-deterministic-simulation-N-daemons-one-seed.md
priority: high
ordinal: 1066000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
The user asked (2026-11-19) for a FoundationDB-style full-stack deterministic simulation. doc-24 is the design: madsim for nodes, time, randomness, network and tonic; per-node statics; threads and runtimes through one seam; disk through Dir (MemDir crashes); an in-process bucket with faults; simulated guests at the daemon's request layer; the local_cell::sim checker lifted to the daemon; fault profiles and rule searches.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Every step in doc-24 done as its own subtask
- [ ] #2 A profile with attrition, network faults and bucket faults runs a few hundred seeds per push in CI, seeds replay, and each rule of bucket_cell::Rules and local_cell::Rules that applies has a search finding its violation
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
