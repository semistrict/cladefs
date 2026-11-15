---
id: TASK-418
title: >-
  sim::deletions_need_records_reread scans ~5,400 seeds (~33 min on a loaded
  Mac) before its first finding
status: To Do
assignee: []
created_date: '2026-11-14 22:01'
labels: []
dependencies: []
priority: medium
type: bug
ordinal: 806000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Seen in TASK-411.8's full extent_store run on 2026-11-14 (base 7f5b5e7f7; Mac load 50-130 from other jobs): the test's first finding seed was 85,424 of its profile, about 5,400 seeds scanned, 33 minutes; with TASK-391's recoveries_need_newest_checkpoints (41 min) they made a run of the crate take 43 minutes. Unchanged with the simulation's blocks at 512 bytes or 4 KiB. The suite's budget and short iteration loops need it back to seconds.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 sim::deletions_need_records_reread finds its case within a time comparable to its neighbours' (under a minute on the Mac), or its search is bounded by a pinned seed
- [ ] #2 The cause of the slowdown, if a change moved the seed, is named in the task
<!-- AC:END -->

## Definition of Done
<!-- DOD:BEGIN -->
- [ ] #1 cargo nextest run --workspace passes on macOS and Linux (ubu1, under fxjob)
- [ ] #2 scripts/upstream-changes.py --check passes
- [ ] #3 Deviations from upstream documented where they occur
<!-- DOD:END -->
