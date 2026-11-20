---
id: TASK-701
title: 'A replicated cell''s keeper holds its store past Uploader::close'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-11-20 19:43'
updated_date: '2026-11-20 19:43'
labels:
  - bug
  - replication
  - madsim
dependencies: []
priority: high
ordinal: 1101000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by the madsim replicated attrition profile (TASK-685.7). Shared::keep, spawned beside the uploader of a replicated cell, holds Arc<Shared>, and so the store, and sleeps KEEPING (250 ms) before it looks at stopping. Uploader::close waits only for the uploader's own task, so OpenCell::create_placed / Opened::close returned with the store still claimed: a guest's OPEN right after a replicated CreateCell (or a close then reopen in one process) failed EBUSY ('this process has the store open'). Fix: Shared::run returns once its keeper is done too (a oneshot the keeper's task drops), and the keeper wakes at a stop (select of its tick and until_stopping).
<!-- SECTION:DESCRIPTION:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
