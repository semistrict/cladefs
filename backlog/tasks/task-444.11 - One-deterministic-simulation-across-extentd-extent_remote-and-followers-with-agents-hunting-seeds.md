---
id: TASK-444.11
title: >-
  One deterministic simulation across extentd, extent_remote and followers, with
  agents hunting seeds
status: To Do
assignee: []
created_date: '2026-11-15 12:43'
labels: []
dependencies: []
references:
  - >-
    backlog/docs/doc-17 -
    Spending-frontier-tokens-on-quality-what-the-forefront-does-and-what-fits-fxdax.md
parent_task_id: TASK-444
priority: low
ordinal: 860000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Each subsystem has its own simulation (TASK-28.1 for object storage, TASK-42.1 for followers, guest/sim for dax.c), but no single process runs them together, so a bug that needs two subsystems to interact (a collector pass during a follower advance during a degraded bucket) gets only what the soak samples. Datadog (2026) scaled one such simulation from 500 seeds to 10M with agents writing workloads and triaging, and caught a WAL bug where an in-memory truncate ran before fsync. S2 uses turmoil and mad-turmoil (MIT) to control time and randomness, plus a linearizability checker. userfaultfd, memfds and the kernel would be stubbed.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 extentd store logic, extent_remote and a follower run in one seeded process with time, randomness, bucket faults and fsync faults under the seed
- [ ] #2 Follower reads are checked for linearizability against the writer history
- [ ] #3 A failing seed replays exactly and is shrunk
- [ ] #4 Seeds run nightly on ubu1 under fxjob; the notes record what they found
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
