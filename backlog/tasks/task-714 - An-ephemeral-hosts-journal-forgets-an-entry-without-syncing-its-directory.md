---
id: TASK-714
title: An ephemeral host's journal forgets an entry without syncing its directory
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 23:39'
updated_date: '2026-11-20 23:40'
labels:
  - bug
  - ephemeral
  - durability
dependencies: []
priority: medium
ordinal: 1117000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-698.5's ephemeral profile on crashing disks (TASK-685.4; e.g. seed 9 of 300: the cell noted on hosts [1, 2], held on 1). Host::forget removes the entry's file and syncs nothing: a crash after brings the entry back, on a host that holds the cell no more, whose boot then retires the reference again (harmless) or, worse, two hosts' journals name the cell. Fix: the journal's directory synced after the removal.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
