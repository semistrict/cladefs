---
id: TASK-713
title: >-
  A migration target's boot gives up a hand-over it accepted, crashed before it
  re-noted the cell
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
priority: high
ordinal: 1116000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-698.5's ephemeral profile once each simulated host's disk crashes at its restart (TASK-685.4; e.g. seed 6 of 300). MigrateHandOver on the target moves the staged store into place, makes it durable, writes the accepted marker, then notes the cell's entry without its migration. Crashed between the marker and the note, the target's boot (Host::retire_stale) finds the entry naming the migration and gives it up: the cell's reference retired, though the source handed the cell over and lets it go: the cell lost. Fix: an entry whose migration the host accepted is noted taken at boot, and kept if held.
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
