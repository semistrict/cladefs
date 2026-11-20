---
id: TASK-715
title: >-
  A live migration's lost prepare or abort leaves the target's journal entry,
  which refuses the cell's later migrations
status: To Do
assignee: []
created_date: '2026-11-20 23:54'
labels:
  - bug
  - ephemeral
  - migration
dependencies: []
priority: high
ordinal: 1118000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-698.5's ephemeral profile once the full-stack simulation's schedules changed (TASK-685.4: each simulated host's disk crashed at its restart; seeds 9, 10, 74, 203, 210, 230, 263 of 300). Two endings: (1) the cell noted on hosts [1, 2], held on 1: the target's journal keeps an entry of a migration that never handed over, while the target runs (its boot alone gives such entries up); (2) host N never answered: a later migration of the cell to that target is refused ALREADY_EXISTS ('cell ... is here', the entry of the older migration), which the source answers as not answered, ENOTCONN, again and again. Suspected: a MigratePrepare whose reply is lost (the source then retries with a new migration ID), or a MigrateAbort not delivered after a refused hand-over, leaves the target holding the older migration; also, asked again after its last files failed, the source's hand-over names no files (files None once the marker is written), which the target's holds() takes as whole. Needs the spec (specs/ephemeral): a target superseding a cell's older, unaccepted migration at the next prepare, or the source aborting by ID what it prepared.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
